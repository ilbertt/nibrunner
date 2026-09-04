//! A checkpoint's filesystem as a block device, for as long as one export needs it.
//!
//! Ported from `apps/agent/src/lib/exports/reader.ts`, which starts the server through a
//! templated systemd unit. This daemon has no systemd, so it starts the process itself and waits
//! for the socket rather than for `systemctl start` to return — the unit's own `ExecStartPost`
//! does exactly that wait, and for the same reason: ZeroFS execs well before it is answering on
//! anything.
//!
//! A checkpoint server is **not** a second writer, which is what makes starting one allowed at
//! all. `--checkpoint` opens read-only against a pinned manifest that no longer advances, so it
//! takes no SlateDB writer epoch and cannot fence the live server.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use protocol::{CheckpointId, VolumeId};

use crate::volumes::nbd::{NbdDevices, NbdTarget};
use crate::volumes::VolumeError;

/// `[servers.nbd] unix_socket` in the checkpoint config, under the checkpoint's own directory.
const NBD_SOCKET_FILENAME: &str = "nbd.sock";

/// The instance name, which is what keeps two servers off each other's socket and cache. ZeroFS
/// expands it in its own config file, so this is the whole of the interface.
const CHECKPOINT_VARIABLE: &str = "NIBRUN_CHECKPOINT";

/// How long a server is given to answer. Generous because it is opening an object store, and
/// bounded because the alternative to giving up is an export that hangs rather than reports.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const READY_POLL: Duration = Duration::from_millis(100);

/// A server that has not gone after this is one this host stops waiting on; the reap will find it.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the read-only servers live, and what they are started with.
pub struct CheckpointServers {
    pub binary: PathBuf,
    pub config_file: PathBuf,
    pub runtime_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl CheckpointServers {
    pub fn socket_path_for(&self, checkpoint_id: &CheckpointId) -> PathBuf {
        self.runtime_dir
            .join(checkpoint_id.as_str())
            .join(NBD_SOCKET_FILENAME)
    }

    fn cache_path_for(&self, checkpoint_id: &CheckpointId) -> PathBuf {
        self.cache_dir.join(checkpoint_id.as_str())
    }

    /// Started for one export's read and stopped when it ends.
    ///
    /// Nothing restarts it. It serves one read, and a server that silently went away and came back
    /// would leave the reader attached across the gap — where one that stays dead is something the
    /// export can notice and report.
    pub async fn start(&self, checkpoint_id: &CheckpointId) -> Result<CheckpointServer, VolumeError> {
        // Scratch for one pass over one filesystem rather than a working set to keep warm, so it
        // is made on the way in and removed on the way out rather than accumulating a directory
        // per export.
        let cache = self.cache_path_for(checkpoint_id);
        crate::json_store::make_directory(&cache, 0o700)
            .map_err(|error| VolumeError::Unusable(error.to_string()))?;
        crate::json_store::make_directory(&self.runtime_dir.join(checkpoint_id.as_str()), 0o750)
            .map_err(|error| VolumeError::Unusable(error.to_string()))?;

        let child = tokio::process::Command::new(&self.binary)
            .arg("run")
            .arg("--config")
            .arg(&self.config_file)
            .arg("--checkpoint")
            .arg(checkpoint_id.as_str())
            .env(CHECKPOINT_VARIABLE, checkpoint_id.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Killed with this daemon rather than outliving it: it holds nothing a restart would
            // want back, unlike a tenant's microVM.
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                VolumeError::Unusable(format!("no checkpoint server could be started: {error}"))
            })?;

        let server = CheckpointServer {
            checkpoint_id: checkpoint_id.clone(),
            socket_path: self.socket_path_for(checkpoint_id),
            cache_dir: cache,
            child,
        };
        server.wait_until_answering().await?;
        Ok(server)
    }
}

/// One running server. Stopping it is `stop`, and dropping it kills the process without the
/// tidying — which is the right way round for a daemon going down in a hurry.
pub struct CheckpointServer {
    checkpoint_id: CheckpointId,
    socket_path: PathBuf,
    cache_dir: PathBuf,
    child: tokio::process::Child,
}

impl CheckpointServer {
    /// The socket appearing is the readiness signal, because the process execs long before it is
    /// answering on anything and there is nothing else to ask.
    async fn wait_until_answering(&self) -> Result<(), VolumeError> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if self.socket_path.exists() {
                return Ok(());
            }
            tokio::time::sleep(READY_POLL).await;
        }
        Err(VolumeError::Unusable(format!(
            "the server for {} did not answer on {} in time",
            self.checkpoint_id,
            self.socket_path.display()
        )))
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Tolerated rather than checked, because this also runs on the way out of an export that
    /// already failed, where an error here would replace the reason it failed with the reason its
    /// cleanup did.
    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(STOP_TIMEOUT, self.child.wait()).await;
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

/// The device a checkpoint is read through, attached for one export and detached after it.
///
/// One device, and so one export reading at a time on a host: a second export attaching a second
/// checkpoint to it would read the first export's filesystem.
pub struct ReaderDevice<'a> {
    devices: &'a NbdDevices,
    device_path: String,
}

impl<'a> ReaderDevice<'a> {
    pub async fn attach(
        devices: &'a NbdDevices,
        socket_path: &Path,
        volume_id: &VolumeId,
    ) -> Result<ReaderDevice<'a>, VolumeError> {
        let device_path = nft_render::export_reader_device_path();
        // Harmless on a device nothing is attached to, and the case it covers is a daemon that
        // died mid-export: the kernel still holds the minor, and an attach over it finds it busy.
        let _ = devices.detach(&device_path).await;
        devices
            .attach_checkpoint(&NbdTarget {
                socket_path: &socket_path.display().to_string(),
                device_path: &device_path,
                volume_id,
            })
            .await?;
        Ok(ReaderDevice { devices, device_path })
    }

    pub fn path(&self) -> &str {
        &self.device_path
    }

    /// The device goes before the server does: taking the socket away from a kernel client that
    /// still holds it is how a detach turns into a hang.
    pub async fn detach(self) {
        let _ = self.devices.detach(&self.device_path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::RecordingCommandRunner;

    fn servers(root: &Path) -> CheckpointServers {
        CheckpointServers {
            // A shell that exits immediately: enough to assert the readiness wait, which is what
            // this daemon does differently from the unit it was ported from.
            binary: PathBuf::from("/usr/bin/true"),
            config_file: root.join("checkpoint.toml"),
            runtime_dir: root.join("run"),
            cache_dir: root.join("cache"),
        }
    }

    #[test]
    fn a_server_answers_on_a_socket_of_its_own_so_two_cannot_fight_over_one_address() {
        let root = tempfile::tempdir().unwrap();
        let one = CheckpointId::parse("export-one").unwrap();
        let two = CheckpointId::parse("export-two").unwrap();
        let servers = servers(root.path());
        assert_ne!(servers.socket_path_for(&one), servers.socket_path_for(&two));
        assert!(servers.socket_path_for(&one).ends_with("export-one/nbd.sock"));
    }

    #[tokio::test]
    async fn a_server_whose_socket_never_appears_is_given_up_on() {
        let root = tempfile::tempdir().unwrap();
        let checkpoint_id = CheckpointId::parse("export-one").unwrap();
        let Err(error) = servers(root.path()).start(&checkpoint_id).await else {
            panic!("a server whose socket never appeared was treated as ready");
        };
        assert!(error.message().contains("did not answer"), "{error}");
    }

    #[tokio::test]
    async fn the_reader_device_is_taken_down_before_it_is_attached() {
        let sysfs = tempfile::tempdir().unwrap();
        let commands = RecordingCommandRunner::succeeding();
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands.clone());
        let volume_id = VolumeId::parse("vol-1").unwrap();

        let reader = ReaderDevice::attach(
            &devices,
            Path::new("/run/zerofs-checkpoint/one/nbd.sock"),
            &volume_id,
        )
        .await
        .unwrap();
        assert_eq!(reader.path(), "/dev/nbd63");
        reader.detach().await;

        let asked: Vec<Vec<String>> = commands.calls().into_iter().map(|call| call.command).collect();
        assert_eq!(
            asked[0],
            vec!["nbd-client".to_string(), "-d".into(), "/dev/nbd63".into()]
        );
        assert!(asked[1].contains(&"/dev/nbd63".to_string()));
        // Never `-persist`: a checkpoint server is started for one export and stopped after it, so
        // one that died mid-read is not coming back.
        assert!(!asked[1].contains(&"-persist".to_string()));
        assert_eq!(
            asked[2],
            vec!["nbd-client".to_string(), "-d".into(), "/dev/nbd63".into()]
        );
    }

    #[test]
    fn the_reader_device_is_not_one_an_app_could_hold() {
        let reserved = nft_render::export_reader_device_path();
        for slot in 0..nft_render::SLOT_COUNT {
            let held = nft_render::describe_slot(slot, crate::test_support::app_id());
            assert_ne!(held.nbd_device_path, reserved);
        }
    }
}
