use std::future::Future;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use protocol::AppId;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OnceCell};

use crate::adapters::cron_execution::{CronExecutionError, CronExitStatus};
use crate::adapters::cron_worker::CronWorkerClient;
use crate::adapters::worker_process::install_program;
use crate::config::HostConfig;
use crate::domain::cron::scheduler::ScheduledRun;
use crate::ports::CronExecution;

const START_TIMEOUT: Duration = Duration::from_secs(10);

pub struct DetachedCronExecution {
    config: HostConfig,
    executable: PathBuf,
    program: OnceCell<PathBuf>,
    current: Mutex<Option<PathBuf>>,
}

impl DetachedCronExecution {
    pub fn new(config: &HostConfig, executable: PathBuf) -> Self {
        Self {
            config: config.clone(),
            executable,
            program: OnceCell::new(),
            current: Mutex::new(None),
        }
    }

    async fn program(&self) -> Result<&PathBuf, CronExecutionError> {
        self.program
            .get_or_try_init(|| async {
                let source = self.executable.clone();
                let directory = self.config.state_dir.join("cron-worker");
                tokio::task::spawn_blocking(move || install_program(&directory, &std::fs::read(source)?))
                    .await
                    .map_err(|_| CronExecutionError::WorkerUnavailable)?
                    .map_err(|_| CronExecutionError::WorkerUnavailable)
            })
            .await
    }

    async fn launch(&self, path: &Path) -> Result<CronWorkerClient, CronExecutionError> {
        let mut command = tokio::process::Command::new(self.program().await?);
        command
            .arg("cron-worker")
            .arg("--socket")
            .arg(path)
            .current_dir("/")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(false);
        command.as_std_mut().process_group(0);
        let mut child = command
            .spawn()
            .map_err(|_| CronExecutionError::WorkerUnavailable)?;
        let started = async {
            {
                let mut input = child
                    .stdin
                    .take()
                    .expect("the worker's stdin was configured as a pipe");
                input.write_all(self.config.to_toml().as_bytes()).await?;
                input.shutdown().await?;
            }
            loop {
                if let Ok(client) = CronWorkerClient::connect(path).await {
                    return Ok::<_, io::Error>(client);
                }
                if child.try_wait()?.is_some() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        match tokio::time::timeout(START_TIMEOUT, started).await {
            Ok(Ok(client)) => Ok(client),
            _ => {
                let _ = child.kill().await;
                Err(CronExecutionError::WorkerUnavailable)
            }
        }
    }

    async fn client(&self) -> Result<CronWorkerClient, CronExecutionError> {
        let mut current = self.current.lock().await;
        if let Some(path) = current.as_ref() {
            if let Ok(client) = CronWorkerClient::connect(path).await {
                return Ok(client);
            }
        }
        let path = self
            .config
            .runtime_dir
            .join("cron")
            .join(format!("worker-{}.sock", uuid::Uuid::new_v4().simple()));
        let client = self.launch(&path).await?;
        *current = Some(path);
        Ok(client)
    }

    async fn workers(&self) -> Result<Vec<CronWorkerClient>, CronExecutionError> {
        let directory = self.config.runtime_dir.join("cron");
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(_) => return Err(CronExecutionError::WorkerUnavailable),
        };
        let mut clients = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| CronExecutionError::WorkerUnavailable)?;
            let path = entry.path();
            if !entry.file_name().as_encoded_bytes().starts_with(b"worker-")
                || !entry
                    .file_type()
                    .map_err(|_| CronExecutionError::WorkerUnavailable)?
                    .is_socket()
            {
                continue;
            }
            match crate::unix_socket::connect(&path).await {
                Ok(stream) => clients.push(CronWorkerClient::from_stream(stream)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    let old = entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age >= START_TIMEOUT);
                    if !old {
                        continue;
                    }
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => return Err(CronExecutionError::WorkerUnavailable),
                    }
                }
                Err(_) => return Err(CronExecutionError::WorkerUnavailable),
            }
        }
        Ok(clients)
    }
}

#[async_trait::async_trait]
impl CronExecution for DetachedCronExecution {
    async fn run<'a>(
        &'a self,
        socket: &'a Path,
        run: ScheduledRun,
        cancellation: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
    ) -> Result<CronExitStatus, CronExecutionError> {
        tokio::select! {
            result = async { self.client().await?.execute(socket, run).await } => result,
            () = cancellation => Err(CronExecutionError::Cancelled),
        }
    }

    async fn busy(&self, app_id: &AppId) -> Result<bool, CronExecutionError> {
        for client in self.workers().await? {
            match tokio::time::timeout(START_TIMEOUT, client.active(app_id)).await {
                Ok(Ok(0)) => {}
                Ok(Ok(_)) => return Ok(true),
                _ => return Err(CronExecutionError::WorkerUnavailable),
            }
        }
        Ok(false)
    }

    async fn stop(&self, app_id: &AppId) -> Result<(), CronExecutionError> {
        for client in self.workers().await? {
            tokio::time::timeout(START_TIMEOUT, client.stop(app_id))
                .await
                .map_err(|_| CronExecutionError::WorkerUnavailable)??;
        }
        Ok(())
    }
}
