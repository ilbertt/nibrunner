use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use protocol::AppId;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OnceCell};

use super::HttpWorkerClient;
use crate::adapters::worker_process::install_program;
use crate::config::HostConfig;
use crate::domain::metrics::proxy::ProxySnapshot;
use crate::domain::report::routes::RouteTarget;

pub struct DetachedHttpProxy {
    config: HostConfig,
    executable: PathBuf,
    program: OnceCell<PathBuf>,
    current: Mutex<Option<PathBuf>>,
    samples: Mutex<BTreeMap<PathBuf, ProxySnapshot>>,
}

impl DetachedHttpProxy {
    pub fn new(config: &HostConfig, executable: PathBuf) -> Self {
        Self {
            config: config.clone(),
            executable,
            program: OnceCell::new(),
            current: Mutex::new(None),
            samples: Mutex::new(BTreeMap::new()),
        }
    }

    async fn program(&self) -> io::Result<&PathBuf> {
        self.program
            .get_or_try_init(|| async {
                let source = self.executable.clone();
                let directory = self.config.state_dir.join("http-worker");
                tokio::task::spawn_blocking(move || install_program(&directory, &std::fs::read(source)?))
                    .await
                    .map_err(io::Error::other)?
            })
            .await
    }

    async fn launch(&self, path: &Path) -> io::Result<HttpWorkerClient> {
        let mut command = tokio::process::Command::new(self.program().await?);
        command
            .arg("http-worker")
            .arg("--socket")
            .arg(path)
            .current_dir("/")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(false);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn()?;
        let starting = async {
            let mut input = child
                .stdin
                .take()
                .expect("the worker receives configuration on a pipe");
            input.write_all(self.config.to_toml().as_bytes()).await?;
            input.shutdown().await?;
            drop(input);
            let client = HttpWorkerClient::new(path);
            loop {
                if client.active().await.is_ok() {
                    return Ok(client);
                }
                if child.try_wait()?.is_some() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        match tokio::time::timeout(super::wire::CONTROL_TIMEOUT, starting).await {
            Ok(Ok(client)) => {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                Ok(client)
            }
            outcome => {
                let _ = child.kill().await;
                Err(match outcome {
                    Ok(Err(error)) => error,
                    _ => io::ErrorKind::TimedOut.into(),
                })
            }
        }
    }

    async fn workers(&self) -> io::Result<Vec<(PathBuf, HttpWorkerClient)>> {
        let entries = match std::fs::read_dir(self.config.runtime_dir.join("http")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error),
        };
        let mut workers = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_name().as_encoded_bytes().starts_with(b"worker-")
                || !entry.file_type()?.is_socket()
            {
                continue;
            }
            let path = entry.path();
            let client = HttpWorkerClient::new(&path);
            match client.active().await {
                Ok(_) => workers.push((path.clone(), client)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    let _ = std::fs::remove_file(path);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(workers)
    }

    async fn listener(&self, workers: &[(PathBuf, HttpWorkerClient)]) -> io::Result<TcpListener> {
        let http = self
            .config
            .proxy
            .http
            .as_ref()
            .ok_or(io::ErrorKind::InvalidInput)?;
        let address = SocketAddr::new(http.listen_address, http.port);
        for (_, worker) in workers {
            if let Ok(listener) = worker.listener().await {
                if listener.local_addr()? == address {
                    return Ok(listener);
                }
            }
        }
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }

    pub async fn apply(&self, routes: Vec<RouteTarget>) -> io::Result<()> {
        let mut current = self.current.lock().await;
        if let Some(path) = current.as_ref() {
            if HttpWorkerClient::new(path).active().await.is_err() {
                *current = None;
            }
        }
        let workers = self.workers().await?;
        if !workers.is_empty() {
            self.metrics().await?;
        }
        if current.is_none() {
            let listener = self.listener(&workers).await?;
            let path = self
                .config
                .runtime_dir
                .join("http")
                .join(format!("worker-{}.sock", uuid::Uuid::new_v4().simple()));
            let worker = self.launch(&path).await?;
            let ready = async {
                worker.apply(routes.clone()).await?;
                worker.attach(listener).await
            }
            .await;
            if let Err(error) = ready {
                let _ = worker.drain().await;
                return Err(error);
            }
            *current = Some(path);
        }
        let path = current
            .as_ref()
            .expect("a successful launch installs the current worker");
        HttpWorkerClient::new(path).apply(routes.clone()).await?;
        for (previous, worker) in workers {
            if &previous != path {
                worker.apply(routes.clone()).await?;
                worker.drain().await?;
            }
        }
        Ok(())
    }

    pub async fn metrics(&self) -> io::Result<Vec<ProxySnapshot>> {
        let workers = self.workers().await?;
        if workers.is_empty() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let mut samples = self.samples.lock().await;
        let mut live = BTreeSet::new();
        for (path, worker) in workers {
            match worker.metrics().await {
                Ok(sample) => {
                    live.insert(path.clone());
                    samples.insert(path, sample);
                }
                Err(error) => {
                    if worker.active().await.is_ok() {
                        return Err(error);
                    }
                }
            }
        }
        for (path, sample) in samples.iter_mut() {
            if !live.contains(path) {
                sample.in_flight = 0;
                for app in sample.apps.values_mut() {
                    app.open = 0;
                }
            }
        }
        Ok(samples.values().cloned().collect())
    }

    pub async fn close_connections_to(&self, app_id: &AppId) -> io::Result<()> {
        for (_, worker) in self.workers().await? {
            worker.close_connections_to(app_id).await?;
        }
        Ok(())
    }

    pub async fn discard(&self, app_id: &AppId) -> io::Result<()> {
        for (_, worker) in self.workers().await? {
            worker.discard(app_id).await?;
        }
        Ok(())
    }

    pub async fn drain(&self) -> io::Result<()> {
        for (_, worker) in self.workers().await? {
            worker.drain().await?;
        }
        Ok(())
    }
}
