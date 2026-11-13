mod client;
pub mod process;
mod wire;

pub use client::HttpWorkerClient;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinSet;

use self::wire::{Reply, Request, CONTROL_TIMEOUT};
use super::http_log::HttpRequestLog;
use super::listener::HttpListener;
use super::{router, RouteTable, Router};
use crate::config::HostConfig;
use crate::domain::metrics::HostMetrics;

struct SocketPath(PathBuf);

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

enum Listening {
    Waiting,
    Serving(std::net::TcpListener),
    Draining,
}

struct Worker {
    router: Arc<Router>,
    metrics: Arc<HostMetrics>,
    tls: Option<tokio_rustls::TlsAcceptor>,
    address: std::net::SocketAddr,
    listener: Mutex<Listening>,
    stop: Notify,
    drained: Notify,
}

impl Worker {
    async fn attach(self: &Arc<Self>, mut stream: BufReader<UnixStream>) -> io::Result<()> {
        let mut state = self.listener.lock().await;
        if !matches!(*state, Listening::Waiting) {
            return wire::write(
                stream.get_mut(),
                &Reply::Failed("the HTTP worker already received its listener".into()),
            )
            .await;
        }
        wire::write(stream.get_mut(), &Reply::Ready).await?;
        let (mut stream, listener) = wire::receive_listener(stream.into_inner()).await?;
        let address = listener.local_addr()?;
        if address.ip() != self.address.ip() || (self.address.port() != 0 && address != self.address) {
            return wire::write(
                &mut stream,
                &Reply::Failed("the listener does not match the worker's configuration".into()),
            )
            .await;
        }
        let serving = HttpListener::from_listener(
            self.router.clone(),
            tokio::net::TcpListener::from_std(listener)?,
            self.tls.clone(),
        )?;
        *state = Listening::Serving(serving.duplicate_listener()?);
        let worker = self.clone();
        tokio::spawn(async move {
            if let Err(error) = serving.serve(worker.stop.notified()).await {
                tracing::error!(%error, "the HTTP worker listener stopped unexpectedly");
            }
            *worker.listener.lock().await = Listening::Draining;
            worker.drained.notify_one();
        });
        wire::write(&mut stream, &Reply::Ready).await
    }

    async fn export(&self, mut stream: BufReader<UnixStream>) -> io::Result<()> {
        let listener = {
            let state = self.listener.lock().await;
            let Listening::Serving(listener) = &*state else {
                return Err(io::ErrorKind::NotConnected.into());
            };
            listener.try_clone()?
        };
        wire::write(stream.get_mut(), &Reply::Ready).await?;
        if stream.read_u8().await? != wire::DESCRIPTOR_READY {
            return Err(io::ErrorKind::InvalidData.into());
        }
        wire::send_listener(stream.into_inner(), listener).await?;
        Ok(())
    }

    async fn answer(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let mut stream = BufReader::new(stream);
        let request = match wire::read::<Request>(&mut stream).await {
            Ok(request) => request,
            Err(_) => {
                return wire::write(stream.get_mut(), &Reply::Failed("invalid worker request".into())).await;
            }
        };
        match request {
            Request::Attach => return self.attach(stream).await,
            Request::Export => return self.export(stream).await,
            Request::Routes(routes) => self.router.apply(RouteTable::from_targets(&routes)).await,
            Request::Close(app_id) => self.router.close_connections_to(&app_id).await,
            Request::Discard(app_id) => self.router.discard_http_log(&app_id).await,
            Request::Active => {
                return wire::write(
                    stream.get_mut(),
                    &Reply::Active(self.metrics.proxy.open_requests()),
                )
                .await;
            }
            Request::Metrics => {
                return wire::write(stream.get_mut(), &Reply::Metrics(self.metrics.proxy.snapshot())).await;
            }
            Request::Drain => {
                let previous = std::mem::replace(&mut *self.listener.lock().await, Listening::Draining);
                match previous {
                    Listening::Waiting => self.drained.notify_one(),
                    Listening::Serving(_) => self.stop.notify_one(),
                    Listening::Draining => {}
                }
            }
        }
        wire::write(stream.get_mut(), &Reply::Ready).await
    }
}

pub async fn serve(config: HostConfig, path: PathBuf) -> io::Result<()> {
    let directory = config.runtime_dir.join("http");
    if path.parent() != Some(directory.as_path()) || path.file_name().is_none() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let http = config.proxy.http.as_ref().ok_or(io::ErrorKind::InvalidInput)?;
    let tls = http
        .tls
        .as_ref()
        .map(|tls| router::tls_acceptor(&tls.certificate, &tls.key, tls.client_ca.as_deref()))
        .transpose()?;
    let metrics = Arc::new(HostMetrics::new());
    let logs = Arc::new(HttpRequestLog::new(config.logs_dir())?);
    let worker = Arc::new(Worker {
        router: Router::with_http_config(metrics.clone(), Some(logs), Some(http)),
        metrics,
        tls,
        address: std::net::SocketAddr::new(http.listen_address, http.port),
        listener: Mutex::new(Listening::Waiting),
        stop: Notify::new(),
        drained: Notify::new(),
    });
    crate::json_store::make_directory(&directory, 0o700)?;
    let listener = crate::unix_socket::bind_private(&path).await?;
    let _path = SocketPath(path);
    let keys = worker.router.clone();
    let refresh = tokio::spawn(async move { keys.refresh_jwks().await });
    let mut requests = JoinSet::new();
    let startup_deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
    loop {
        let waiting = matches!(*worker.listener.lock().await, Listening::Waiting);
        tokio::select! {
            _ = worker.drained.notified() => break,
            _ = tokio::time::sleep_until(startup_deadline), if waiting => {
                if matches!(*worker.listener.lock().await, Listening::Waiting) {
                    break;
                }
            }
            Some(_) = requests.join_next(), if !requests.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::warn!(%error, "the HTTP worker could not accept a control connection");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let worker = worker.clone();
                requests.spawn(async move {
                    let result = tokio::time::timeout(CONTROL_TIMEOUT, worker.answer(stream)).await;
                    match result {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => tracing::warn!(%error, "HTTP worker control request failed"),
                        Err(_) => tracing::warn!("HTTP worker control request timed out"),
                    }
                });
            }
        }
    }
    drop(listener);
    refresh.abort();
    while requests.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests;
