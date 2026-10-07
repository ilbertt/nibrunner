use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};

use crate::ports::VmError;

const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

// PID 1 opens this port after mounting its filesystems and preparing the tenant cgroup.
pub(super) async fn initialized(path: &Path, active: impl Fn() -> bool) -> Result<(), VmError> {
    tokio::time::timeout(INITIALIZATION_TIMEOUT, async {
        loop {
            if !active() {
                return Err(VmError::Host(
                    "the microVM exited before its guest initialized".into(),
                ));
            }
            match connect(path).await {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(VmError::Host(error.to_string()));
                }
                Err(_) => tokio::time::sleep(RETRY_INTERVAL).await,
            }
        }
    })
    .await
    .map_err(|_| VmError::Host("the guest did not initialize within 60s".into()))?
}

async fn connect(path: &Path) -> std::io::Result<()> {
    let mut wire = BufReader::new(crate::unix_socket::connect(path).await?);
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

    #[tokio::test(start_paused = true)]
    async fn a_guest_that_never_initializes_has_a_bounded_wait() {
        let directory = tempfile::tempdir().unwrap();
        let error = initialized(&directory.path().join("missing"), || true)
            .await
            .unwrap_err();
        assert!(error.message().contains("within 60s"));
    }

    #[tokio::test]
    async fn a_vm_that_exits_during_initialization_ends_the_wait() {
        let directory = tempfile::tempdir().unwrap();
        let active = AtomicBool::new(true);
        let socket = directory.path().join("missing");
        let waiting = initialized(&socket, || active.load(Ordering::Relaxed));
        tokio::pin!(waiting);
        tokio::select! {
            result = &mut waiting => panic!("initialization ended before exit: {result:?}"),
            () = tokio::time::sleep(RETRY_INTERVAL) => active.store(false, Ordering::Relaxed),
        }
        assert!(waiting.await.unwrap_err().message().contains("exited"));
    }
}
