use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use protocol::AppId;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::wire::{self, Reply, Request, CONTROL_TIMEOUT};
use crate::domain::report::routes::RouteTarget;

#[derive(Clone)]
pub struct HttpWorkerClient {
    path: PathBuf,
}

impl HttpWorkerClient {
    pub fn new(path: &Path) -> Self {
        Self { path: path.into() }
    }

    async fn request(&self, message: Request) -> io::Result<(BufReader<UnixStream>, Reply)> {
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            let mut stream = crate::unix_socket::connect(&self.path).await?;
            wire::write(&mut stream, &message).await?;
            let mut stream = BufReader::new(stream);
            let reply = wire::read(&mut stream).await?;
            Ok((stream, reply))
        })
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
    }

    fn ready(reply: Reply) -> io::Result<()> {
        match reply {
            Reply::Ready => Ok(()),
            Reply::Failed(message) => Err(io::Error::other(message)),
            Reply::Active(_) => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    async fn acknowledged(&self, message: Request) -> io::Result<()> {
        Self::ready(self.request(message).await?.1)
    }

    pub async fn apply(&self, routes: Vec<RouteTarget>) -> io::Result<()> {
        self.acknowledged(Request::Routes(routes)).await
    }

    pub async fn active(&self) -> io::Result<BTreeMap<AppId, u64>> {
        match self.request(Request::Active).await?.1 {
            Reply::Active(apps) => Ok(apps),
            Reply::Failed(message) => Err(io::Error::other(message)),
            Reply::Ready => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    pub async fn drain(&self) -> io::Result<()> {
        self.acknowledged(Request::Drain).await
    }

    pub async fn close_connections_to(&self, app_id: &AppId) -> io::Result<()> {
        self.acknowledged(Request::Close(app_id.clone())).await
    }

    pub async fn discard(&self, app_id: &AppId) -> io::Result<()> {
        self.acknowledged(Request::Discard(app_id.clone())).await
    }

    pub async fn attach(&self, listener: std::net::TcpListener) -> io::Result<()> {
        let (stream, reply) = self.request(Request::Attach).await?;
        Self::ready(reply)?;
        let stream = wire::send_listener(stream.into_inner(), listener).await?;
        Self::ready(wire::read(&mut BufReader::new(stream)).await?)
    }

    pub async fn listener(&self) -> io::Result<std::net::TcpListener> {
        let (mut stream, reply) = self.request(Request::Export).await?;
        Self::ready(reply)?;
        // A normal read can consume the descriptor's payload and discard its ancillary data.
        stream.write_all(b"R").await?;
        wire::receive_listener(stream.into_inner())
            .await
            .map(|(_, listener)| listener)
    }
}
