use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use protocol::{CheckpointId, VolumeId};

use crate::adapters::volumes::nbd::{NbdDevices, NbdTarget};
use crate::adapters::volumes::VolumeError;

const NBD_SOCKET_FILENAME: &str = "nbd.sock";

const CHECKPOINT_VARIABLE: &str = "NIBRUN_CHECKPOINT";

pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(10);

// Opening a checkpoint reads it out of the object store, so how long a server takes to answer is
// a property of the store rather than of this host.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(60);
const ATTACH_POLL: Duration = Duration::from_millis(500);
const READY_POLL: Duration = Duration::from_millis(100);

const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct CheckpointServers {
    pub binary: PathBuf,
    pub config_file: PathBuf,
    pub runtime_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub ready_timeout: Duration,
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

    pub async fn start(&self, checkpoint_id: &CheckpointId) -> Result<CheckpointServer, VolumeError> {
        let cache = self.cache_path_for(checkpoint_id);
        crate::json_store::make_directory(&cache, 0o700)
            .map_err(|error| VolumeError::Unusable(error.to_string()))?;
        crate::json_store::make_directory(&self.runtime_dir.join(checkpoint_id.as_str()), 0o750)
            .map_err(|error| VolumeError::Unusable(error.to_string()))?;
        // A server that died leaves its socket behind, and readiness here is the socket appearing.
        // Left in place, the wait below is answered by the last run's socket before this one is
        // listening, and the attach that follows is refused by a server that is not there yet.
        let _ = std::fs::remove_file(self.socket_path_for(checkpoint_id));

        let child = tokio::process::Command::new(&self.binary)
            .arg("run")
            .arg("--config")
            .arg(&self.config_file)
            .arg("--checkpoint")
            .arg(checkpoint_id.as_str())
            .env(CHECKPOINT_VARIABLE, checkpoint_id.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Kept rather than discarded: this process is the only one that knows why a
            // checkpoint would not open, and a server that never answers is otherwise a timeout
            // with no reason attached to it.
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                VolumeError::Unusable(format!("no checkpoint server could be started: {error}"))
            })?;

        let mut child = child;
        let said = child.stderr.take();
        let mut server = CheckpointServer {
            checkpoint_id: checkpoint_id.clone(),
            socket_path: self.socket_path_for(checkpoint_id),
            cache_dir: cache,
            child,
            ready_timeout: self.ready_timeout,
        };
        if let Err(error) = server.wait_until_answering().await {
            let _ = server.child.kill().await;
            return Err(match complained(said).await {
                Some(reason) => VolumeError::Unusable(format!("{}: {reason}", error.message())),
                None => error,
            });
        }
        Ok(server)
    }
}

// What the server managed to say before it was given up on. It is killed first so the read ends,
// and a server that said nothing leaves the timeout to speak for itself.
async fn complained(said: Option<tokio::process::ChildStderr>) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut said = said?;
    let mut text = String::new();
    // Only ever on the way to an error, so it can afford to wait out a loaded machine rather
    // than lose the one sentence that explains the failure.
    tokio::time::timeout(Duration::from_secs(5), said.read_to_string(&mut text))
        .await
        .ok()?
        .ok()?;
    let last = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .next_back()?;
    Some(last.chars().take(300).collect())
}

pub struct CheckpointServer {
    checkpoint_id: CheckpointId,
    socket_path: PathBuf,
    cache_dir: PathBuf,
    child: tokio::process::Child,
    ready_timeout: Duration,
}

impl CheckpointServer {
    async fn wait_until_answering(&self) -> Result<(), VolumeError> {
        let deadline = tokio::time::Instant::now() + self.ready_timeout;
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

    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(STOP_TIMEOUT, self.child.wait()).await;
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

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
        let _ = devices.detach(&device_path).await;
        let target = NbdTarget {
            socket_path: &socket_path.display().to_string(),
            device_path: &device_path,
            volume_id,
        };
        // The server binds its socket before it has opened the checkpoint, so a socket that is
        // there is not a server that will answer, and the only honest test of readiness is the
        // thing being waited for. It refuses in milliseconds while it is still opening, so this
        // asks again rather than deciding on the first answer.
        let deadline = tokio::time::Instant::now() + ATTACH_TIMEOUT;
        let mut refusal = devices.attach_checkpoint(&target).await;
        while refusal.is_err() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(ATTACH_POLL).await;
            let _ = devices.detach(&device_path).await;
            refusal = devices.attach_checkpoint(&target).await;
        }
        refusal?;
        Ok(ReaderDevice { devices, device_path })
    }

    pub fn path(&self) -> &str {
        &self.device_path
    }

    pub async fn detach(self) {
        let _ = self.devices.detach(&self.device_path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mocks;

    fn servers(root: &Path) -> CheckpointServers {
        servers_waiting(root, Duration::from_secs(5))
    }

    fn servers_waiting(root: &Path, ready_timeout: Duration) -> CheckpointServers {
        CheckpointServers {
            binary: PathBuf::from("/usr/bin/true"),
            config_file: root.join("checkpoint.toml"),
            runtime_dir: root.join("run"),
            cache_dir: root.join("cache"),
            ready_timeout,
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
        let Err(error) = servers_waiting(root.path(), Duration::from_millis(200))
            .start(&checkpoint_id)
            .await
        else {
            panic!("a server whose socket never appeared was treated as ready");
        };
        assert!(error.message().contains("did not answer"), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_gave_up_hands_on_what_it_said_rather_than_only_that_it_was_late() {
        let root = tempfile::tempdir().unwrap();
        let checkpoint_id = CheckpointId::parse("export-one").unwrap();
        // Long enough that the server is certainly killed after it has spoken rather than before:
        // one killed first says nothing, which is a real outcome but not the one under test.
        let mut servers = servers_waiting(root.path(), Duration::from_secs(2));
        // A binary that refuses out loud and never opens a socket, which is a checkpoint that is
        // not there as far as this end can tell.
        let refuses = root.path().join("refuses");
        std::fs::write(
            &refuses,
            "#!/bin/sh\necho \"Error: Checkpoint not found\" >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&refuses, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        servers.binary = refuses;

        let Err(error) = servers.start(&checkpoint_id).await else {
            panic!("a server that refused was treated as ready");
        };
        assert!(error.message().contains("did not answer"), "{error}");
        assert!(
            error.message().contains("Checkpoint not found"),
            "the reason the server gave is missing: {error}"
        );
    }

    #[tokio::test]
    async fn a_socket_the_last_server_left_behind_is_not_mistaken_for_this_one_answering() {
        let root = tempfile::tempdir().unwrap();
        let checkpoint_id = CheckpointId::parse("export-one").unwrap();
        let servers = servers_waiting(root.path(), Duration::from_millis(200));

        // What a server that died leaves: its directory and its socket, with nothing listening.
        let socket = servers.socket_path_for(&checkpoint_id);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, b"").unwrap();

        let Err(error) = servers.start(&checkpoint_id).await else {
            panic!("the last run's socket was taken for this run's server");
        };
        assert!(error.message().contains("did not answer"), "{error}");
    }

    #[tokio::test]
    async fn an_attach_the_server_is_not_ready_for_yet_is_asked_again() {
        let sysfs = tempfile::tempdir().unwrap();
        // Refuses the first attach the way a server still opening its checkpoint does, then takes
        // it. Anything that gave up on the first answer would never reach the second.
        let refused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let once = refused.clone();
        let (commands, log) = mocks::commands_answering(move |request| {
            let attaching =
                request.executable() == "nbd-client" && request.command.iter().any(|word| word == "-N");
            if attaching && !once.swap(true, std::sync::atomic::Ordering::Relaxed) {
                return Err(crate::ports::CommandError::Failed {
                    executable: "nbd-client".to_string(),
                    code: 1,
                    reason: ": Exiting.".to_string(),
                });
            }
            Ok(crate::ports::CommandResult::succeeded())
        });
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands);
        let volume_id = VolumeId::parse("vol-1").unwrap();

        let reader = ReaderDevice::attach(
            &devices,
            Path::new("/run/zerofs-checkpoint/one/nbd.sock"),
            &volume_id,
        )
        .await
        .expect("the second attach is taken");
        assert_eq!(reader.path(), "/dev/nbd63");
        assert!(
            log.commands()
                .iter()
                .filter(|call| call[0] == "nbd-client")
                .count()
                >= 3,
            "the attach was not asked again: {:?}",
            log.commands()
        );
    }

    #[tokio::test]
    async fn the_reader_device_is_taken_down_before_it_is_attached() {
        let sysfs = tempfile::tempdir().unwrap();
        let (commands, log) = mocks::commands_succeeding();
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands);
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

        let asked = log.commands();
        assert_eq!(
            asked[0],
            vec!["nbd-client".to_string(), "-d".into(), "/dev/nbd63".into()]
        );
        assert!(asked[1].contains(&"/dev/nbd63".to_string()));
        assert!(!asked[1].contains(&"-persist".to_string()));
        assert_eq!(
            asked[2],
            vec!["nbd-client".to_string(), "-d".into(), "/dev/nbd63".into()]
        );
    }

    fn server_that_comes_up(root: &Path, socket_path: &Path) -> PathBuf {
        let binary = root.join("fake-checkpoint-server");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s' \"${CHECKPOINT_VARIABLE}\" > {}\ntouch {}\nexec sleep 5\n",
                root.join("asked-for").display(),
                socket_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        binary
    }

    #[tokio::test]
    async fn a_server_that_came_up_answers_where_it_was_told_to_and_takes_its_cache_down_with_it() {
        let root = tempfile::tempdir().unwrap();
        let checkpoint_id = CheckpointId::parse("export-one").unwrap();
        let mut servers = servers(root.path());
        let socket_path = servers.socket_path_for(&checkpoint_id);
        servers.binary = server_that_comes_up(root.path(), &socket_path);

        let server = servers.start(&checkpoint_id).await.unwrap();
        assert_eq!(server.socket_path(), socket_path);
        assert_eq!(
            std::fs::read_to_string(root.path().join("asked-for")).unwrap(),
            "export-one"
        );
        let cache = servers.cache_dir.join(checkpoint_id.as_str());
        assert!(cache.is_dir());

        server.stop().await;
        assert!(!cache.exists());
    }

    #[test]
    fn two_checkpoints_never_share_the_cache_one_of_them_would_have_to_clear() {
        let root = tempfile::tempdir().unwrap();
        let servers = servers(root.path());
        let one = CheckpointId::parse("export-one").unwrap();
        let two = CheckpointId::parse("export-two").unwrap();
        assert_ne!(servers.cache_path_for(&one), servers.cache_path_for(&two));
        assert!(servers.cache_path_for(&one).starts_with(&servers.cache_dir));
    }

    #[test]
    fn the_reader_device_is_not_one_an_app_could_hold() {
        let reserved = nft_render::export_reader_device_path();
        for slot in 0..nft_render::NBD_SLOT_LIMIT {
            let held = nft_render::describe_slot(slot, crate::test_support::app_id());
            assert_ne!(held.nbd_device_path, reserved);
        }
    }
}
