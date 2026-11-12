use std::collections::BTreeMap;
use std::io;
use std::net::TcpListener;
use std::os::unix::net::UnixStream as BlockingStream;
use std::time::Duration;

use protocol::AppId;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::domain::report::routes::RouteTarget;

pub(super) const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const MESSAGE_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Routes(Vec<RouteTarget>),
    Attach,
    Export,
    Active,
    Drain,
    Close(AppId),
    Discard(AppId),
}

#[derive(Serialize, Deserialize)]
pub(super) enum Reply {
    Ready,
    Active(BTreeMap<AppId, u64>),
    Failed(String),
}

pub(super) async fn read<T: DeserializeOwned>(stream: &mut BufReader<UnixStream>) -> io::Result<T> {
    let mut line = Vec::new();
    (&mut *stream)
        .take(MESSAGE_LIMIT)
        .read_until(b'\n', &mut line)
        .await?;
    if !line.ends_with(b"\n") {
        return Err(io::ErrorKind::InvalidData.into());
    }
    serde_json::from_slice(&line).map_err(|_| io::ErrorKind::InvalidData.into())
}

pub(super) async fn write<T: Serialize>(stream: &mut UnixStream, message: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await
}

async fn blocking<T: Send + 'static>(
    stream: UnixStream,
    transfer: impl FnOnce(&BlockingStream) -> io::Result<T> + Send + 'static,
) -> io::Result<(UnixStream, T)> {
    let stream = stream.into_std()?;
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_nonblocking(false)?;
    let (stream, value) = tokio::task::spawn_blocking(move || {
        let value = transfer(&stream)?;
        stream.set_nonblocking(true)?;
        Ok::<_, io::Error>((stream, value))
    })
    .await
    .map_err(io::Error::other)??;
    Ok((UnixStream::from_std(stream)?, value))
}

pub(super) async fn send_listener(stream: UnixStream, listener: TcpListener) -> io::Result<UnixStream> {
    blocking(stream, move |channel| {
        super::super::listener_handoff::send(channel, &listener)
    })
    .await
    .map(|(stream, ())| stream)
}

pub(super) async fn receive_listener(stream: UnixStream) -> io::Result<(UnixStream, TcpListener)> {
    blocking(stream, super::super::listener_handoff::receive).await
}
