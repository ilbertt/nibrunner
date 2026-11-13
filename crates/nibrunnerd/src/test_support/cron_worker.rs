use std::path::{Path, PathBuf};

use chrono::Utc;
use guest_contract::cron_execution::{self as wire, ExecutionFrame};
use protocol::{CronCommand, CronJobDefinition, CronSchedule};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::config::HostConfig;
use crate::domain::cron::scheduler::{JobKey, ScheduledRun};

pub fn run() -> ScheduledRun {
    ScheduledRun {
        key: JobKey {
            app_id: super::app_id(),
            deployment_id: super::deployment_id(),
            index: 0,
        },
        job: CronJobDefinition {
            schedule: CronSchedule::parse("@daily").unwrap(),
            command: CronCommand::parse("printf 'before\\n'; printf 'after\\n'").unwrap(),
            environment: None,
        },
        scheduled_at: Utc::now(),
    }
}

pub fn address(config: &HostConfig) -> PathBuf {
    config.runtime_dir.join("cron/test.sock")
}

pub async fn active(path: &Path) -> usize {
    crate::adapters::cron_worker::CronWorkerClient::connect(path)
        .await
        .unwrap()
        .active(&super::app_id())
        .await
        .unwrap()
}

#[derive(Debug)]
enum Completion {
    Finish,
    AwaitCancellation,
}

pub struct HoldingGuest {
    began: oneshot::Receiver<()>,
    release: oneshot::Sender<Completion>,
    server: tokio::task::JoinHandle<()>,
}

async fn send(stream: &mut BufReader<UnixStream>, frame: ExecutionFrame) {
    stream
        .get_mut()
        .write_all(&wire::encode_reply(&frame).unwrap())
        .await
        .unwrap();
    if !matches!(frame, ExecutionFrame::Exit { .. }) {
        assert_eq!(stream.read_u8().await.unwrap(), wire::ACK);
    }
}

impl HoldingGuest {
    pub async fn new(path: &Path) -> Self {
        crate::json_store::make_directory(path.parent().unwrap(), 0o700).unwrap();
        let listener = UnixListener::bind(path).unwrap();
        let (began, started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut connect = String::new();
            stream.read_line(&mut connect).await.unwrap();
            assert_eq!(
                connect,
                guest_contract::vsock::connect_request(guest_contract::vsock::CRON_EXECUTION_PORT)
            );
            stream.get_mut().write_all(b"OK 1234\n").await.unwrap();
            let mut header = [0; wire::HEADER_BYTES];
            stream.read_exact(&mut header).await.unwrap();
            let header = wire::decode_request_header(&header).unwrap();
            let mut body = vec![0; header.body_length];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(
                wire::decode_request(header, &body).unwrap().command,
                run().job.command.expose()
            );
            send(&mut stream, ExecutionFrame::Started).await;
            send(&mut stream, ExecutionFrame::Stdout(b"before\n".to_vec())).await;
            began.send(()).unwrap();
            let completion = tokio::select! {
                biased;
                requested = released => requested.unwrap(),
                result = stream.read_u8() => panic!("the execution channel changed before release: {result:?}"),
            };
            match completion {
                Completion::Finish => {
                    send(&mut stream, ExecutionFrame::Stdout(b"after\n".to_vec())).await;
                    send(&mut stream, ExecutionFrame::Exit { code: 0, signal: 0 }).await;
                }
                Completion::AwaitCancellation => {
                    assert_eq!(
                        stream.read_u8().await.unwrap_err().kind(),
                        std::io::ErrorKind::UnexpectedEof
                    );
                }
            }
            assert_eq!(
                listener.into_std().unwrap().accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        Self {
            began: started,
            release,
            server,
        }
    }

    pub async fn started(&mut self) {
        (&mut self.began).await.unwrap();
    }

    pub fn expect_cancellation(self) -> tokio::task::JoinHandle<()> {
        self.release.send(Completion::AwaitCancellation).unwrap();
        self.server
    }

    pub async fn finish(self) {
        self.release.send(Completion::Finish).unwrap();
        self.server.await.unwrap();
    }
}
