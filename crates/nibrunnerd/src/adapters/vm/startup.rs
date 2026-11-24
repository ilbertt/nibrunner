use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};

use super::CONTROL_RETRY_INTERVAL;

pub(super) async fn sample(path: &Path) -> Option<crate::ports::GuestStartupSample> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let socket = crate::unix_socket::connect(path).await.ok()?;
        let mut wire = BufReader::new(socket);
        let port = guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT;
        wire.get_mut()
            .write_all(guest_contract::vsock::connect_request(port).as_bytes())
            .await
            .ok()?;
        let reply = crate::domain::guest_line::read(&mut wire, CONNECT_TIMEOUT).await?;
        guest_contract::vsock::read_connect_reply(&reply, port).ok()?;
        let requested = std::time::Instant::now();
        wire.get_mut()
            .write_all(format!("{}\n", guest_contract::startup::STARTUP_REQUEST).as_bytes())
            .await
            .ok()?;
        let reply = crate::domain::guest_line::read(&mut wire, CONNECT_TIMEOUT).await?;
        let received_at = std::time::Instant::now();
        Some(crate::ports::GuestStartupSample {
            guest: guest_contract::startup::StartupSnapshot::from_reply(&reply)?,
            received_at,
            round_trip: received_at.duration_since(requested),
        })
    })
    .await
    .ok()
    .flatten()
}

const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub(super) enum InitializationError {
    #[error("the microVM exited before its guest initialized")]
    Exited,
    #[error("permission was denied connecting to the guest control socket")]
    PermissionDenied,
    #[error("the guest did not initialize within 60s")]
    TimedOut,
}

// PID 1 opens this port after mounting its filesystems and preparing the tenant cgroup.
pub(super) async fn initialized(
    path: &Path,
    jail_uid: u32,
    active: impl Fn() -> bool,
) -> Result<(), InitializationError> {
    tokio::time::timeout(INITIALIZATION_TIMEOUT, async {
        loop {
            if !active() {
                return Err(InitializationError::Exited);
            }
            match connect(path, jail_uid).await {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(InitializationError::PermissionDenied);
                }
                Err(_) => tokio::time::sleep(CONTROL_RETRY_INTERVAL).await,
            }
        }
    })
    .await
    .map_err(|_| InitializationError::TimedOut)?
}

async fn connect(path: &Path, jail_uid: u32) -> std::io::Result<()> {
    // The host creates the directory as root before the jailer takes ownership.
    // Only the launch UID is stable throughout the first connection attempt.
    let mut wire = BufReader::new(crate::unix_socket::connect_to_uid(path, jail_uid).await?);
    wire.get_mut()
        .write_all(
            guest_contract::vsock::connect_request(guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT)
                .as_bytes(),
        )
        .await?;
    let reply = crate::domain::guest_line::read(&mut wire, CONNECT_TIMEOUT)
        .await
        .ok_or_else(|| std::io::Error::other("the guest control port has not answered"))?;
    guest_contract::vsock::read_connect_reply(&reply, guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn readiness_rejects_a_peer_that_owns_the_directory_but_lacks_the_launch_uid() {
        use std::os::unix::fs::MetadataExt;
        use tokio::io::AsyncReadExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let socket = directory.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let directory_uid = directory.path().metadata().unwrap().uid();
        let error = connect(&socket, directory_uid ^ 1).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let (mut connection, _) = listener.accept().await.unwrap();
        assert_eq!(connection.read(&mut [0]).await.unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn guest_control_readiness_is_retried_before_ten_milliseconds_pass() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("missing");
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        let starting = tokio::spawn(async move {
            initialized(&socket, 0, || {
                observed.fetch_add(1, Ordering::Relaxed);
                true
            })
            .await
        });
        while attempts.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(8)).await;
        tokio::task::yield_now().await;
        assert!(attempts.load(Ordering::Relaxed) > 1);
        starting.abort();
        assert!(starting.await.unwrap_err().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_that_never_initializes_has_a_bounded_wait() {
        let directory = tempfile::tempdir().unwrap();
        let error = initialized(&directory.path().join("missing"), 0, || true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("within 60s"));
    }

    #[tokio::test]
    async fn a_vm_that_exits_during_initialization_ends_the_wait() {
        let directory = tempfile::tempdir().unwrap();
        let active = AtomicBool::new(true);
        let socket = directory.path().join("missing");
        let waiting = initialized(&socket, 0, || active.load(Ordering::Relaxed));
        tokio::pin!(waiting);
        tokio::select! {
            result = &mut waiting => panic!("initialization ended before exit: {result:?}"),
            () = tokio::time::sleep(CONTROL_RETRY_INTERVAL) => active.store(false, Ordering::Relaxed),
        }
        assert!(waiting.await.unwrap_err().to_string().contains("exited"));
    }
}
