use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::ports::VmError;

const TIMEOUT: Duration = Duration::from_millis(guest_contract::control::TENANT_CONTROL_TIMEOUT_MS);

fn failed(reason: impl std::fmt::Display) -> VmError {
    VmError::Host(format!("guest clock synchronization failed: {reason}"))
}

async fn line(wire: &mut BufReader<UnixStream>) -> Result<String, VmError> {
    crate::domain::guest_line::read(wire, TIMEOUT)
        .await
        .ok_or_else(|| failed("guest control reply was missing, invalid or timed out"))
}

async fn connect(path: &Path) -> Result<BufReader<UnixStream>, VmError> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let stream = loop {
        match crate::unix_socket::connect(path).await {
            Ok(stream) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Err(failed(error)),
            Err(error) if tokio::time::Instant::now() < deadline => {
                tracing::debug!(%error, socket = %path.display(), "waiting for guest control socket");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(failed(error)),
        }
    };
    let mut wire = BufReader::new(stream);
    wire.get_mut()
        .write_all(
            guest_contract::vsock::connect_request(guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT)
                .as_bytes(),
        )
        .await
        .map_err(failed)?;
    guest_contract::vsock::read_connect_reply(
        &line(&mut wire).await?,
        guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT,
    )
    .map_err(failed)?;
    Ok(wire)
}

pub(super) async fn freeze_tenant(path: &Path) -> Result<(), VmError> {
    tokio::time::timeout(TIMEOUT, freeze_connected_tenant(path))
        .await
        .map_err(|_| failed("tenant freeze timed out"))?
}

async fn freeze_connected_tenant(path: &Path) -> Result<(), VmError> {
    let mut wire = connect(path).await?;
    wire.get_mut()
        .write_all(format!("{}\n", guest_contract::control::TENANT_FREEZE_REQUEST).as_bytes())
        .await
        .map_err(failed)?;
    if line(&mut wire).await? != guest_contract::control::TENANT_FREEZE_READY {
        return Err(failed("the guest does not support tenant freezing"));
    }
    wire.get_mut()
        .write_all(format!("{}\n", guest_contract::control::TENANT_FREEZE_COMMIT).as_bytes())
        .await
        .map_err(failed)?;
    if line(&mut wire).await? != guest_contract::control::TENANT_FREEZE_HELD {
        return Err(failed("the tenant cgroup would not freeze"));
    }
    Ok(())
}

pub(super) async fn wake(path: &Path) -> Result<(), VmError> {
    tokio::time::timeout(TIMEOUT, release_connected_tenant(path))
        .await
        .map_err(|_| failed("guest clock synchronization timed out"))?
}

async fn release_connected_tenant(path: &Path) -> Result<(), VmError> {
    let mut wire = connect(path).await?;
    wire.get_mut()
        .write_all(format!("{}\n", guest_contract::control::TENANT_CLOCK_REQUEST).as_bytes())
        .await
        .map_err(failed)?;
    let reply = line(&mut wire).await?;
    if reply != guest_contract::control::TENANT_CLOCK_READY {
        return Err(failed(&reply));
    }
    let sent = SystemTime::now().duration_since(UNIX_EPOCH).map_err(failed)?;
    wire.get_mut()
        .write_all(
            format!(
                "{}{}\n",
                guest_contract::control::TENANT_CLOCK_RELEASE,
                sent.as_nanos()
            )
            .as_bytes(),
        )
        .await
        .map_err(failed)?;
    if line(&mut wire).await? != guest_contract::control::TENANT_CLOCK_RELEASED {
        return Err(failed("the tenant cgroup was not released"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixListener;

    async fn guest(listener: UnixListener, ready: bool) -> Option<String> {
        let (stream, _) = listener
            .accept()
            .await
            .expect("guest control socket accepts the host");
        let mut wire = BufReader::new(stream);
        assert_eq!(line(&mut wire).await.unwrap(), "CONNECT 51001");
        wire.get_mut().write_all(b"OK 1234\n").await.unwrap();
        let request = line(&mut wire).await.unwrap();
        assert_eq!(request, guest_contract::control::TENANT_CLOCK_REQUEST);
        wire.get_mut()
            .write_all(if ready { b"READY\n" } else { b"REFUSED\n" })
            .await
            .unwrap();
        let mut reply = String::new();
        wire.read_line(&mut reply).await.unwrap();
        if reply.starts_with(guest_contract::control::TENANT_CLOCK_RELEASE) {
            wire.get_mut().write_all(b"OK\n").await.unwrap();
            Some(reply)
        } else {
            None
        }
    }

    #[tokio::test]
    async fn a_restored_guest_is_released_after_its_clock_is_set() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.vsock");
        let listener = UnixListener::bind(&socket).unwrap();
        let answer = tokio::spawn(guest(listener, true));

        wake(&socket).await.unwrap();

        let release = answer.await.unwrap().unwrap();
        let timestamp = release
            .trim()
            .strip_prefix(guest_contract::control::TENANT_CLOCK_RELEASE)
            .unwrap()
            .parse::<u128>()
            .unwrap();
        assert!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                .abs_diff(timestamp)
                < Duration::from_secs(1).as_nanos()
        );
    }

    #[tokio::test]
    async fn the_host_samples_time_after_a_delayed_guest_says_ready() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.vsock");
        let listener = UnixListener::bind(&socket).unwrap();
        let answer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = BufReader::new(stream);
            assert_eq!(line(&mut wire).await.unwrap(), "CONNECT 51001");
            wire.get_mut().write_all(b"OK 1234\n").await.unwrap();
            assert_eq!(line(&mut wire).await.unwrap(), "WAKE");
            tokio::time::sleep(Duration::from_millis(50)).await;
            let ready_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            wire.get_mut().write_all(b"READY\n").await.unwrap();
            let release = line(&mut wire).await.unwrap();
            let timestamp = release.strip_prefix("GO ").unwrap().parse::<u128>().unwrap();
            assert!(timestamp >= ready_at);
            wire.get_mut().write_all(b"OK\n").await.unwrap();
        });
        wake(&socket).await.unwrap();
        answer.await.unwrap();
    }

    #[tokio::test]
    async fn a_guest_that_cannot_set_its_clock_is_not_released() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.vsock");
        let listener = UnixListener::bind(&socket).unwrap();
        let answer = tokio::spawn(guest(listener, false));

        let failure = wake(&socket).await.unwrap_err();

        assert!(failure.message().contains("REFUSED"));
        assert_eq!(answer.await.unwrap(), None);
    }
}
