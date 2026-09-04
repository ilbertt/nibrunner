//! Holding a tenant's filesystem still for exactly as long as it takes to cut a checkpoint.
//!
//! Ported from `apps/agent/src/lib/exports/freeze.ts`. The connection *is* the lease: dropping it
//! is what tells the guest to thaw, so a daemon that dies mid-export cannot leave a tenant's
//! filesystem wedged behind it.

use std::path::Path;
use std::time::Duration;

use protocol::AppId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const FREEZE_REQUEST: &str = "FREEZE\n";
const FREEZE_HELD: &str = "OK";

/// The guest answers a freeze once ext4 has checkpointed its journal, which is work rather than a
/// round trip.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum FreezeError {
    #[error("{socket_path} took the request and never answered")]
    Silent { socket_path: String },
    #[error("the guest running {app_id} would not freeze its filesystem: {reply}")]
    Refused { app_id: AppId, reply: String },
    /// The guest thaws itself if a freeze is held past its own ceiling, and a checkpoint cut
    /// across that moment is worthless: the tenant was writing while it was taken, so it is
    /// neither the state before nor the state after. Reported as a failed export rather than read
    /// from, because the point of freezing is that nobody has to wonder afterwards.
    #[error("the guest running {app_id} thawed before the checkpoint was recorded")]
    Lost { app_id: AppId },
}

impl FreezeError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// A held freeze, for as long as this is alive.
///
/// A stopped app has no VMM to ask and needs none: its guest unmounted the filesystem on the way
/// down, which is the same journal checkpoint under another name. A VMM that is running and does
/// not answer is the case that must not be waved through — that is a guest whose journal still
/// holds writes the bundle would otherwise miss without saying so.
pub struct FreezeLease {
    app_id: AppId,
    /// `None` where there was no guest to hold. Kept open for the life of the lease and never
    /// read from again: what matters is that this side has not let go.
    held: Option<UnixStream>,
}

impl FreezeLease {
    /// Whether the guest was still frozen. Ask before trusting anything taken while it was.
    ///
    /// A guest that thawed early closes its end, which this side sees as a readable socket at
    /// end-of-file. Nothing is ever sent on this connection after the reply, so anything readable
    /// on it is the far end having gone.
    pub fn assert_held(&self) -> Result<(), FreezeError> {
        let Some(stream) = &self.held else {
            return Ok(());
        };
        let mut byte = [0u8; 1];
        match stream.try_read(&mut byte) {
            // Would block: nothing to read, so the far end is still there and still holding.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            // Zero bytes is end-of-file, and anything else is a guest that said something it was
            // never going to say. Both are a lease this side can no longer vouch for.
            _ => Err(FreezeError::Lost {
                app_id: self.app_id.clone(),
            }),
        }
    }
}

/// The tenant's filesystem, held still. Nothing to hold where no VMM is listening.
pub async fn frozen(app_id: &AppId, vsock_path: &Path) -> Result<FreezeLease, FreezeError> {
    let Ok(stream) = UnixStream::connect(vsock_path).await else {
        tracing::info!(
            %app_id,
            socket_path = %vsock_path.display(),
            "no running guest to freeze; reading the volume as it lies"
        );
        return Ok(FreezeLease {
            app_id: app_id.clone(),
            held: None,
        });
    };
    let socket_path = vsock_path.display().to_string();
    let silent = || FreezeError::Silent {
        socket_path: socket_path.clone(),
    };

    let mut wire = BufReader::new(stream);
    let connect = guest_contract::vsock::connect_request(guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT);
    write_all(&mut wire, connect.as_bytes())
        .await
        .map_err(|()| silent())?;
    let reply = read_line(&mut wire).await.ok_or_else(silent)?;
    guest_contract::vsock::read_connect_reply(&reply, guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT)
        .map_err(|error| FreezeError::Refused {
            app_id: app_id.clone(),
            reply: error.to_string(),
        })?;

    write_all(&mut wire, FREEZE_REQUEST.as_bytes())
        .await
        .map_err(|()| silent())?;
    let reply = read_line(&mut wire).await.ok_or_else(silent)?;
    if reply != FREEZE_HELD {
        return Err(FreezeError::Refused {
            app_id: app_id.clone(),
            reply,
        });
    }
    tracing::info!(%app_id, "the guest froze its filesystem");
    Ok(FreezeLease {
        app_id: app_id.clone(),
        held: Some(wire.into_inner()),
    })
}

async fn write_all(wire: &mut BufReader<UnixStream>, bytes: &[u8]) -> Result<(), ()> {
    wire.get_mut().write_all(bytes).await.map_err(|_| ())
}

async fn read_line(wire: &mut BufReader<UnixStream>) -> Option<String> {
    let mut line = String::new();
    match tokio::time::timeout(REPLY_TIMEOUT, wire.read_line(&mut line)).await {
        Ok(Ok(read)) if read > 0 => Some(line.trim_end().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::app_id;
    use tokio::net::UnixListener;

    /// A guest that answers the connect and the freeze, then holds the connection open.
    async fn guest_that(
        answers: &'static [&'static str],
        hangs_up: bool,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("guest.vsock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let mut wire = BufReader::new(stream);
            for answer in answers {
                let mut request = String::new();
                if wire.read_line(&mut request).await.unwrap_or(0) == 0 {
                    return;
                }
                let _ = wire.get_mut().write_all(format!("{answer}\n").as_bytes()).await;
            }
            if hangs_up {
                return;
            }
            // Holds the lease open until the far end drops it, which is what a frozen guest does.
            std::future::pending::<()>().await;
        });
        (directory, path)
    }

    /// A stopped app unmounted its filesystem on the way down, which is the same journal
    /// checkpoint under another name — so there is nothing to hold and nothing to wait for.
    #[tokio::test]
    async fn no_vmm_to_ask_is_a_lease_over_nothing_rather_than_a_failure() {
        let directory = tempfile::tempdir().unwrap();
        let lease = frozen(&app_id(), &directory.path().join("nothing-here.vsock"))
            .await
            .unwrap();
        assert!(lease.assert_held().is_ok());
    }

    #[tokio::test]
    async fn a_guest_that_takes_the_freeze_holds_it_until_this_side_lets_go() {
        let (_directory, path) = guest_that(&["OK 1234", "OK"], false).await;
        let lease = frozen(&app_id(), &path).await.unwrap();
        assert!(lease.assert_held().is_ok());
    }

    /// A checkpoint cut across a thaw is neither the state before nor the state after, so a lease
    /// this side cannot vouch for has to say so rather than be read from.
    #[tokio::test]
    async fn a_guest_that_thawed_early_is_a_lease_that_cannot_be_vouched_for() {
        let (_directory, path) = guest_that(&["OK 1234", "OK"], true).await;
        let lease = frozen(&app_id(), &path).await.unwrap();
        // The guest's end is gone the moment it answered; the socket reads end-of-file.
        tokio::task::yield_now().await;
        for _ in 0..50 {
            if lease.assert_held().is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("a guest that hung up was still reported as holding the freeze");
    }

    #[tokio::test]
    async fn a_guest_that_refuses_is_not_read_from() {
        let (_directory, path) = guest_that(&["OK 1234", "BUSY"], false).await;
        let Err(error) = frozen(&app_id(), &path).await else {
            panic!("a guest that refused the freeze was read from anyway");
        };
        assert!(matches!(error, FreezeError::Refused { .. }), "{error}");
        assert!(error.message().contains("BUSY"));
    }
}
