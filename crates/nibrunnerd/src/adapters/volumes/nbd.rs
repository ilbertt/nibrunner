use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::VolumeId;

use crate::adapters::volumes::VolumeError;
use crate::ports::{CommandRequest, CommandRunner};

const NBD_CLIENT: &str = "nbd-client";

const NBD_CONNECTIONS: u32 = 4;
const NBD_BLOCK_SIZE_BYTES: u32 = 4096;

const NBD_TIMEOUT_SECONDS: u64 = 600;

const SYSFS_BLOCK_DIRECTORY: &str = "/sys/block";
const SYSFS_SECTOR_BYTES: u64 = 512;

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the O_DIRECT read uses it")
)]
const PROBE_BYTES: usize = 4096;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

const NO_BYTES: u64 = 0;

pub struct NbdDevices {
    sysfs_block: PathBuf,
    commands: Arc<dyn CommandRunner>,
}

impl NbdDevices {
    pub fn new(commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            sysfs_block: PathBuf::from(SYSFS_BLOCK_DIRECTORY),
            commands,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_sysfs(sysfs_block: PathBuf, commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            sysfs_block,
            commands,
        }
    }

    fn attribute(&self, device_path: &str, attribute: &str) -> Option<String> {
        let name = Path::new(device_path).file_name()?;
        std::fs::read_to_string(self.sysfs_block.join(name).join(attribute)).ok()
    }

    pub fn is_attached(&self, device_path: &str) -> bool {
        self.attribute(device_path, "pid").is_some()
    }

    pub fn attached_size_bytes(&self, device_path: &str) -> u64 {
        self.attribute(device_path, "size")
            .and_then(|sectors| sectors.trim().parse::<u64>().ok())
            .map_or(NO_BYTES, |sectors| sectors.saturating_mul(SYSFS_SECTOR_BYTES))
    }

    pub async fn is_usable(&self, device_path: &str) -> bool {
        if self.attached_size_bytes(device_path) == NO_BYTES {
            return false;
        }
        reads_first_block(device_path).await
    }

    pub async fn attach(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        self.connect(target, &["-persist"]).await
    }

    pub async fn attach_checkpoint(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        self.connect(target, &[]).await
    }

    async fn connect(&self, target: &NbdTarget<'_>, extra: &[&str]) -> Result<(), VolumeError> {
        let timeout = NBD_TIMEOUT_SECONDS.to_string();
        let connections = NBD_CONNECTIONS.to_string();
        let block_size = NBD_BLOCK_SIZE_BYTES.to_string();
        let mut command = vec![
            NBD_CLIENT,
            "-unix",
            target.socket_path,
            target.device_path,
            "-N",
            target.volume_id.as_str(),
        ];
        command.extend_from_slice(extra);
        command.extend_from_slice(&[
            "-timeout",
            &timeout,
            "-connections",
            &connections,
            "-block-size",
            &block_size,
        ]);
        self.commands
            .stdout_of(CommandRequest::new(&command))
            .await
            .map(|_| ())
            .map_err(|error| VolumeError::Unusable(error.message()))
    }

    pub async fn detach(&self, device_path: &str) -> Result<(), VolumeError> {
        self.commands
            .run(CommandRequest::new(&[NBD_CLIENT, "-d", device_path]))
            .await
            .map(|_| ())
            .map_err(|error| VolumeError::Unusable(error.message()))
    }

    pub async fn reattach(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        if self.is_attached(target.device_path) {
            let _ = self.detach(target.device_path).await;
        }
        self.attach(target).await
    }
}

pub struct NbdTarget<'a> {
    pub socket_path: &'a str,
    pub device_path: &'a str,
    pub volume_id: &'a VolumeId,
}

async fn reads_first_block(device_path: &str) -> bool {
    let path = device_path.to_string();
    let read = tokio::task::spawn_blocking(move || direct_read(&path));
    matches!(tokio::time::timeout(PROBE_TIMEOUT, read).await, Ok(Ok(true)))
}

#[cfg(target_os = "linux")]
fn direct_read(device_path: &str) -> bool {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let Ok(mut device) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(device_path)
    else {
        return false;
    };
    let mut buffer = vec![0u8; PROBE_BYTES * 2];
    let offset = buffer.as_ptr().align_offset(PROBE_BYTES);
    if offset > PROBE_BYTES {
        return false;
    }
    device
        .read_exact(&mut buffer[offset..offset + PROBE_BYTES])
        .is_ok()
}

#[cfg(not(target_os = "linux"))]
fn direct_read(_device_path: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::RecordingCommandRunner;

    fn devices(sysfs: &Path) -> (NbdDevices, Arc<RecordingCommandRunner>) {
        let commands = RecordingCommandRunner::succeeding();
        (
            NbdDevices::with_sysfs(sysfs.to_path_buf(), commands.clone()),
            commands,
        )
    }

    fn asked(commands: &RecordingCommandRunner) -> Vec<Vec<String>> {
        commands.calls().into_iter().map(|call| call.command).collect()
    }

    fn attribute(sysfs: &Path, device: &str, attribute: &str, value: &str) {
        let directory = sysfs.join(device);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(attribute), value).unwrap();
    }

    #[tokio::test]
    async fn a_device_is_attached_when_the_kernel_says_something_holds_it() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        assert!(!devices.is_attached("/dev/nbd0"));
        attribute(sysfs.path(), "nbd0", "pid", "4123\n");
        assert!(devices.is_attached("/dev/nbd0"));
    }

    #[tokio::test]
    async fn size_is_sectors_and_a_device_nothing_holds_is_zero() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        assert_eq!(devices.attached_size_bytes("/dev/nbd0"), 0);
        attribute(sysfs.path(), "nbd0", "size", "524288\n");
        assert_eq!(devices.attached_size_bytes("/dev/nbd0"), 524_288 * 512);
        attribute(sysfs.path(), "nbd1", "size", "not a number");
        assert_eq!(devices.attached_size_bytes("/dev/nbd1"), 0);
    }

    #[tokio::test]
    async fn a_device_of_no_size_is_unusable_without_the_device_being_opened() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        attribute(sysfs.path(), "nbd0", "size", "0\n");
        assert!(!devices.is_usable("/dev/nbd0").await);
    }

    #[tokio::test]
    async fn an_attach_names_the_export_the_socket_and_the_ceilings() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        devices
            .attach(&NbdTarget {
                socket_path: "/run/zerofs/nbd.sock",
                device_path: "/dev/nbd0",
                volume_id: &volume_id,
            })
            .await
            .unwrap();
        let asked = asked(&commands);
        assert_eq!(
            asked,
            vec![vec![
                "nbd-client".to_string(),
                "-unix".into(),
                "/run/zerofs/nbd.sock".into(),
                "/dev/nbd0".into(),
                "-N".into(),
                "vol-1".into(),
                "-persist".into(),
                "-timeout".into(),
                "600".into(),
                "-connections".into(),
                "4".into(),
                "-block-size".into(),
                "4096".into(),
            ]]
        );
    }

    #[tokio::test]
    async fn a_checkpoint_is_attached_without_persist() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        devices
            .attach_checkpoint(&NbdTarget {
                socket_path: "/run/zerofs-checkpoint/one/nbd.sock",
                device_path: "/dev/nbd63",
                volume_id: &volume_id,
            })
            .await
            .unwrap();
        assert!(!asked(&commands)[0].contains(&"-persist".to_string()));
    }

    #[tokio::test]
    async fn a_reattach_takes_a_held_device_down_first_and_leaves_an_unheld_one_alone() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        let target = NbdTarget {
            socket_path: "/run/zerofs/nbd.sock",
            device_path: "/dev/nbd0",
            volume_id: &volume_id,
        };
        devices.reattach(&target).await.unwrap();
        assert_eq!(
            asked(&commands).len(),
            1,
            "nothing held it, so nothing to take down"
        );

        attribute(sysfs.path(), "nbd0", "pid", "4123");
        devices.reattach(&target).await.unwrap();
        let asked = asked(&commands);
        assert_eq!(asked.len(), 3);
        assert_eq!(
            asked[1],
            vec!["nbd-client".to_string(), "-d".into(), "/dev/nbd0".into()]
        );
    }
}
