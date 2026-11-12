pub mod process;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::AppId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinSet;

use crate::adapters::cron_execution::{CronExecutionError, CronExitStatus, GuestCronExecution};
use crate::adapters::logs::FileLogSink;
use crate::config::HostConfig;
use crate::domain::cron::scheduler::ScheduledRun;

const START_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
// JSON escapes can expand one byte of the bounded guest request to six bytes.
const REQUEST_LIMIT: u64 = (guest_contract::cron_execution::MAX_REQUEST_BYTES * 6 + 4096) as u64;

#[derive(Serialize, Deserialize)]
enum Request {
    Status { app_id: AppId },
    Stop { app_id: AppId },
    Run { socket: PathBuf, run: ScheduledRun },
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Active(usize),
    Completed(CronExitStatus),
    Failed(String),
    Stopped,
}

async fn read_json<T: serde::de::DeserializeOwned>(stream: &mut BufReader<UnixStream>) -> io::Result<T> {
    let mut line = Vec::new();
    (&mut *stream)
        .take(REQUEST_LIMIT)
        .read_until(b'\n', &mut line)
        .await?;
    if !line.ends_with(b"\n") {
        return Err(io::ErrorKind::InvalidData.into());
    }
    serde_json::from_slice(&line).map_err(|_| io::ErrorKind::InvalidData.into())
}

async fn write_json<T: Serialize>(stream: &mut UnixStream, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await
}

pub struct CronWorkerClient {
    stream: BufReader<UnixStream>,
}

impl CronWorkerClient {
    pub async fn connect(path: &Path) -> Result<Self, CronExecutionError> {
        let stream = crate::unix_socket::connect(path)
            .await
            .map_err(|_| CronExecutionError::WorkerUnavailable)?;
        Ok(Self::from_stream(stream))
    }

    pub(crate) fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }

    pub async fn execute(
        mut self,
        socket: &Path,
        run: ScheduledRun,
    ) -> Result<CronExitStatus, CronExecutionError> {
        tokio::time::timeout(
            START_TIMEOUT,
            write_json(
                self.stream.get_mut(),
                &Request::Run {
                    socket: socket.to_owned(),
                    run,
                },
            ),
        )
        .await
        .map_err(|_| CronExecutionError::WorkerUnavailable)?
        .map_err(|_| CronExecutionError::WorkerUnavailable)?;
        match read_json(&mut self.stream)
            .await
            .map_err(|_| CronExecutionError::WorkerUnavailable)?
        {
            Reply::Completed(status) => Ok(status),
            Reply::Failed(message) => Err(CronExecutionError::WorkerFailed(message)),
            _ => Err(CronExecutionError::WorkerUnavailable),
        }
    }

    pub async fn active(mut self, app_id: &AppId) -> Result<usize, CronExecutionError> {
        write_json(
            self.stream.get_mut(),
            &Request::Status {
                app_id: app_id.clone(),
            },
        )
        .await
        .map_err(|_| CronExecutionError::WorkerUnavailable)?;
        match read_json(&mut self.stream)
            .await
            .map_err(|_| CronExecutionError::WorkerUnavailable)?
        {
            Reply::Active(count) => Ok(count),
            _ => Err(CronExecutionError::WorkerUnavailable),
        }
    }
    pub async fn stop(mut self, app_id: &AppId) -> Result<(), CronExecutionError> {
        write_json(
            self.stream.get_mut(),
            &Request::Stop {
                app_id: app_id.clone(),
            },
        )
        .await
        .map_err(|_| CronExecutionError::WorkerUnavailable)?;
        match read_json(&mut self.stream)
            .await
            .map_err(|_| CronExecutionError::WorkerUnavailable)?
        {
            Reply::Stopped => Ok(()),
            _ => Err(CronExecutionError::WorkerUnavailable),
        }
    }
}

pub async fn serve(config: HostConfig, path: PathBuf) -> io::Result<()> {
    let directory = config.runtime_dir.join("cron");
    if path.parent() != Some(directory.as_path()) || path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cron worker sockets belong in paths.runtime_dir/cron",
        ));
    }
    crate::json_store::make_directory(path.parent().expect("a worker socket has a directory"), 0o700)?;
    let listener = crate::unix_socket::bind_private(&path).await?;
    let sink = Arc::new(FileLogSink::new(
        config.logs_dir(),
        config.logs.keep_bytes_per_app,
    ));
    let (stopping, stop) = tokio::sync::watch::channel(false);
    let signals = tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                signal.recv().await;
            }
        }
        stopping.send_replace(true);
    });
    let runs = Arc::new(crate::domain::cron::runs::CronRuns::default());
    let mut jobs = JoinSet::new();
    let mut responses = JoinSet::new();
    let mut idle_since = tokio::time::Instant::now();
    let mut shutdown = stop.clone();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep_until(idle_since + IDLE_TIMEOUT), if jobs.is_empty() => break,
            Some(_) = jobs.join_next(), if !jobs.is_empty() => { idle_since = tokio::time::Instant::now(); },
            connection = listener.accept() => {
                let (stream, _) = match connection {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::warn!(%error, "the cron worker could not accept a control connection");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let mut stream = BufReader::new(stream);
                let request = tokio::time::timeout(START_TIMEOUT, read_json::<Request>(&mut stream)).await;
                match request {
                    Ok(Ok(Request::Status { app_id })) => { let _ = write_json(stream.get_mut(), &Reply::Active(runs.active_for(&app_id))).await; }
                    Ok(Ok(Request::Stop { app_id })) => {
                        runs.stop(&app_id).await;
                        let _ = write_json(stream.get_mut(), &Reply::Stopped).await;
                    }
                    Ok(Ok(Request::Run { socket, run })) => {
                        let Some(mut lease) = runs.start_owned(&run.key.app_id, &run.key.deployment_id).await else {
                            let _ = write_json(stream.get_mut(), &Reply::Failed("invalid execution request".into())).await;
                            continue;
                        };
                        let sink = sink.clone();
                        let (sender, received) = tokio::sync::oneshot::channel();
                        jobs.spawn(async move {
                            let outcome = GuestCronExecution::run(&socket, run, sink.as_ref(), lease.cancelled()).await;
                            let reply = outcome.map_or_else(|error| Reply::Failed(error.to_string()), Reply::Completed);
                            let _ = sender.send(reply);
                        });
                        responses.spawn(async move { if let Ok(reply) = received.await { let _ = write_json(stream.get_mut(), &reply).await; } });
                    }
                    _ => { let _ = write_json(stream.get_mut(), &Reply::Failed("invalid execution request".into())).await; }
                }
                while responses.try_join_next().is_some() {}
            }
        }
    }
    signals.abort();
    runs.synchronize(&[]).await;
    while jobs.join_next().await.is_some() {}
    while responses.join_next().await.is_some() {}
    let _ = std::fs::remove_file(path);
    Ok(())
}
