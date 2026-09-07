use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DesiredVolume, ObjectKey, VolumeId};
use tokio::sync::Mutex;

use crate::adapters::net::allocator::SlotAllocator;
use crate::adapters::volumes::nbd::{NbdDevices, NbdTarget};
use crate::adapters::volumes::{
    align_to_sector, has_ext_magic, AttachedVolume, CacheReservation, ObservedBacking, VolumeBackend,
    VolumeError, FILESYSTEM_LABEL, SUPERBLOCK_MAGIC_OFFSET,
};
use crate::ports::{CommandRequest, CommandRunner};

pub const NBD_DIRECTORY: &str = ".nbd";

const BYTES_PER_CONFIGURED_GB: u64 = 1_073_741_824;

const CACHE_DISK_SETTING: &str = "disk_size_gb";
const CACHE_MEMORY_SETTING: &str = "memory_size_gb";

const ASSUMED_CACHE_BYTES: u64 = 2048 * 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZerofsFilesystem {
    pub storage_prefix: ObjectKey,
    pub mount_path: PathBuf,
    pub nbd_socket_path: PathBuf,
    pub checkpoint_runtime_dir: PathBuf,
    pub binary: PathBuf,
    pub config_file: PathBuf,
}

impl ZerofsFilesystem {
    pub fn device_file_for(&self, volume_id: &VolumeId) -> PathBuf {
        self.mount_path.join(NBD_DIRECTORY).join(volume_id.as_str())
    }

    pub fn nbd_directory(&self) -> PathBuf {
        self.mount_path.join(NBD_DIRECTORY)
    }
}

pub struct ZerofsVolumes {
    filesystem: ZerofsFilesystem,
    devices: NbdDevices,
    allocator: Arc<Mutex<SlotAllocator>>,
    commands: Arc<dyn CommandRunner>,
}

impl ZerofsVolumes {
    pub fn new(
        filesystem: ZerofsFilesystem,
        allocator: Arc<Mutex<SlotAllocator>>,
        commands: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            filesystem,
            devices: NbdDevices::new(commands.clone()),
            allocator,
            commands,
        }
    }

    #[cfg(test)]
    fn with_devices(mut self, devices: NbdDevices) -> Self {
        self.devices = devices;
        self
    }

    async fn admin(&self, args: &[&str]) -> Result<String, VolumeError> {
        let binary = self.filesystem.binary.display().to_string();
        let config = self.filesystem.config_file.display().to_string();
        let mut command = vec![binary.as_str()];
        command.extend_from_slice(args);
        command.extend_from_slice(&["-c", config.as_str()]);
        self.commands
            .stdout_of(CommandRequest::new(&command))
            .await
            .map_err(|error| VolumeError::Unusable(error.message()))
    }

    async fn device_for(&self, app_id: &protocol::AppId) -> Option<String> {
        self.allocator
            .lock()
            .await
            .lookup(app_id)
            .map(|slot| slot.nbd_device_path)
    }

    fn ensure_device_file(&self, volume_id: &VolumeId, size_bytes: u64) -> Result<u64, VolumeError> {
        let path = self.filesystem.device_file_for(volume_id);
        let target = align_to_sector(size_bytes);
        if let Some(current) = device_file_size(&path) {
            if current > target {
                return Err(VolumeError::ShrinkRefused {
                    current,
                    requested: target,
                });
            }
            if current == target {
                return Ok(target);
            }
        }
        crate::json_store::make_directory(&self.filesystem.nbd_directory(), 0o700)
            .map_err(|error| VolumeError::Unusable(error.to_string()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| VolumeError::Unusable(error.to_string()))?;
        file.set_len(target)
            .map_err(|error| VolumeError::Unusable(error.to_string()))?;
        Ok(target)
    }

    async fn is_formatted(&self, device_path: &str) -> Result<bool, VolumeError> {
        let path = device_path.to_string();
        let unreadable = || VolumeError::SuperblockUnreadable {
            device_path: device_path.to_string(),
        };
        let read = tokio::task::spawn_blocking(move || read_superblock_magic(&path));
        match tokio::time::timeout(SUPERBLOCK_READ_TIMEOUT, read).await {
            Ok(Ok(Some(magic))) => Ok(has_ext_magic(&magic)),
            _ => Err(unreadable()),
        }
    }

    async fn format_once(&self, device_path: &str) -> Result<bool, VolumeError> {
        if self.is_formatted(device_path).await? {
            return Ok(false);
        }
        self.commands
            .stdout_of(CommandRequest::new(&[
                "mke2fs",
                "-q",
                "-t",
                "ext4",
                "-L",
                FILESYSTEM_LABEL,
                device_path,
            ]))
            .await
            .map_err(|error| VolumeError::Unusable(error.message()))?;
        Ok(true)
    }

    pub fn cache_disk_bytes(&self) -> Option<u64> {
        self.cache_bytes(CACHE_DISK_SETTING)
    }

    pub fn cache_memory_bytes(&self) -> Option<u64> {
        self.cache_bytes(CACHE_MEMORY_SETTING)
    }

    fn cache_bytes(&self, setting: &str) -> Option<u64> {
        let text = std::fs::read_to_string(&self.filesystem.config_file).ok()?;
        cache_gigabytes(&text, setting).map(|gigabytes| gigabytes * BYTES_PER_CONFIGURED_GB)
    }
}

const SUPERBLOCK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn device_file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|info| info.is_file())
        .map(|info| info.len())
}

fn read_superblock_magic(device_path: &str) -> Option<[u8; 2]> {
    use std::io::{Read, Seek, SeekFrom};
    let mut device = std::fs::File::open(device_path).ok()?;
    device.seek(SeekFrom::Start(SUPERBLOCK_MAGIC_OFFSET)).ok()?;
    let mut magic = [0u8; 2];
    match device.read_exact(&mut magic) {
        Ok(()) => Some(magic),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Some([0, 0]),
        Err(_) => None,
    }
}

pub fn cache_gigabytes(config: &str, setting: &str) -> Option<u64> {
    let document: toml::Value = toml::from_str(config).ok()?;
    let configured = document.get("cache")?.get(setting)?;
    let gigabytes = configured.as_integer().map_or_else(
        || {
            configured
                .as_float()
                .filter(|value| *value > 0.0)
                .map(|value| value as u64)
        },
        |value| u64::try_from(value).ok(),
    )?;
    (gigabytes > 0).then_some(gigabytes)
}

pub fn parse_checkpoint_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[async_trait]
impl VolumeBackend for ZerofsVolumes {
    async fn provision(&self, desired: &DesiredVolume) -> Result<AttachedVolume, VolumeError> {
        let size_bytes = self.ensure_device_file(&desired.volume_id, desired.size_bytes)?;
        let slot = self
            .allocator
            .lock()
            .await
            .allocate(&desired.app_id)
            .map_err(|error| VolumeError::Unusable(error.message()))?;
        let socket_path = self.filesystem.nbd_socket_path.display().to_string();
        let target = NbdTarget {
            socket_path: &socket_path,
            device_path: &slot.nbd_device_path,
            volume_id: &desired.volume_id,
        };
        if !self.devices.is_usable(&slot.nbd_device_path).await {
            self.devices.reattach(&target).await?;
        }
        if self.format_once(&slot.nbd_device_path).await? {
            tracing::info!(
                volume_id = %desired.volume_id,
                device = %slot.nbd_device_path,
                "volume formatted"
            );
        }
        Ok(AttachedVolume {
            volume_id: desired.volume_id.clone(),
            device_path: slot.nbd_device_path,
            size_bytes,
            storage_prefix: self.filesystem.storage_prefix.clone(),
        })
    }

    async fn attach(
        &self,
        volume_id: &VolumeId,
        app_id: &protocol::AppId,
    ) -> Result<AttachedVolume, VolumeError> {
        let path = self.filesystem.device_file_for(volume_id);
        let Some(size_bytes) = device_file_size(&path) else {
            return Err(VolumeError::NotHere {
                volume_id: volume_id.clone(),
            });
        };
        let device_path = self
            .device_for(app_id)
            .await
            .ok_or_else(|| VolumeError::NotHere {
                volume_id: volume_id.clone(),
            })?;
        let socket_path = self.filesystem.nbd_socket_path.display().to_string();
        let target = NbdTarget {
            socket_path: &socket_path,
            device_path: &device_path,
            volume_id,
        };
        if !self.devices.is_usable(&device_path).await {
            self.devices.reattach(&target).await?;
        }
        Ok(AttachedVolume {
            volume_id: volume_id.clone(),
            device_path,
            size_bytes,
            storage_prefix: self.filesystem.storage_prefix.clone(),
        })
    }

    async fn detach(&self, _volume_id: &VolumeId, app_id: &protocol::AppId) -> Result<(), VolumeError> {
        let Some(device_path) = self.device_for(app_id).await else {
            return Ok(());
        };
        self.devices.detach(&device_path).await
    }

    async fn teardown(&self, volume_id: &VolumeId, app_id: &protocol::AppId) -> Result<(), VolumeError> {
        let _ = self.flush().await;
        let _ = self.detach(volume_id, app_id).await;
        let path = self.filesystem.device_file_for(volume_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(VolumeError::Unusable(error.to_string())),
        }
    }

    async fn flush(&self) -> Result<(), VolumeError> {
        self.admin(&["flush"]).await.map(|_| ())
    }

    async fn create_checkpoint(&self, checkpoint_id: &protocol::CheckpointId) -> Result<(), VolumeError> {
        self.admin(&["checkpoint", "create", checkpoint_id.as_str()])
            .await
            .map(|_| ())
    }

    async fn delete_checkpoint(&self, checkpoint_id: &protocol::CheckpointId) -> Result<(), VolumeError> {
        self.admin(&["checkpoint", "delete", checkpoint_id.as_str()])
            .await
            .map(|_| ())
    }

    async fn observe_checkpoints(&self) -> Vec<protocol::CheckpointId> {
        let Ok(listed) = self.admin(&["checkpoint", "list"]).await else {
            return Vec::new();
        };
        parse_checkpoint_names(&listed)
            .into_iter()
            .filter_map(|name| protocol::CheckpointId::parse(&name).ok())
            .collect()
    }

    fn reserved_cache(&self) -> CacheReservation {
        let assumed = |setting: &str, taken: Option<u64>| match taken {
            Some(bytes) => bytes,
            None => {
                tracing::warn!(
                    config_file = %self.filesystem.config_file.display(),
                    setting,
                    assumed_bytes = ASSUMED_CACHE_BYTES,
                    "the zerofs cache size could not be read; assuming what its config holds today"
                );
                ASSUMED_CACHE_BYTES
            }
        };
        CacheReservation {
            disk_bytes: assumed(CACHE_DISK_SETTING, self.cache_disk_bytes()),
            memory_bytes: assumed(CACHE_MEMORY_SETTING, self.cache_memory_bytes()),
        }
    }

    async fn observe(
        &self,
        owners: &std::collections::BTreeMap<VolumeId, protocol::AppId>,
    ) -> Vec<ObservedBacking> {
        let Ok(entries) = std::fs::read_dir(self.filesystem.nbd_directory()) else {
            tracing::warn!(
                directory = %self.filesystem.nbd_directory().display(),
                "the zerofs nbd directory would not list; this host reports no volumes"
            );
            return Vec::new();
        };
        let mut observed = Vec::new();
        for entry in entries.flatten() {
            let Ok(volume_id) = VolumeId::parse(entry.file_name().to_string_lossy().as_ref()) else {
                continue;
            };
            let Some(size_bytes) = device_file_size(&entry.path()) else {
                continue;
            };
            let device_path = match owners.get(&volume_id) {
                Some(app_id) => self.device_for(app_id).await,
                None => None,
            };
            let attached = match &device_path {
                Some(path) => self.devices.is_usable(path).await,
                None => false,
            };
            observed.push(ObservedBacking {
                volume_id,
                size_bytes,
                attached,
                device_path,
                storage_prefix: self.filesystem.storage_prefix.clone(),
            });
        }
        observed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::RecordingCommandRunner;
    use crate::test_support::{app_id, desired_volume};

    fn filesystem(root: &Path) -> ZerofsFilesystem {
        ZerofsFilesystem {
            storage_prefix: ObjectKey::parse("volumes").unwrap(),
            mount_path: root.join("mnt"),
            nbd_socket_path: PathBuf::from("/run/zerofs/nbd.sock"),
            checkpoint_runtime_dir: PathBuf::from("/run/zerofs-checkpoint"),
            binary: PathBuf::from("/opt/nibrun/bin/zerofs/zerofs"),
            config_file: root.join("zerofs.toml"),
        }
    }

    #[test]
    fn a_gigabyte_is_read_as_a_gibibyte_which_is_the_larger_of_the_two_it_could_mean() {
        let config = "[cache]\ndisk_size_gb = 64\nmemory_size_gb = 8\n";
        assert_eq!(cache_gigabytes(config, "disk_size_gb"), Some(64));
        assert_eq!(cache_gigabytes(config, "memory_size_gb"), Some(8));
    }

    #[test]
    fn a_cache_size_that_is_not_there_or_not_positive_is_not_guessed() {
        assert_eq!(
            cache_gigabytes("[cache]\ndisk_size_gb = 0\n", "disk_size_gb"),
            None
        );
        assert_eq!(cache_gigabytes("[cache]\n", "disk_size_gb"), None);
        assert_eq!(cache_gigabytes("not toml at all {", "disk_size_gb"), None);
        assert_eq!(
            cache_gigabytes("[cache]\ndisk_size_gb = -4\n", "disk_size_gb"),
            None
        );
    }

    #[test]
    fn checkpoint_names_are_the_first_word_of_each_line() {
        let listed = "one   2026-09-04\ntwo   2026-09-05\n\n";
        assert_eq!(parse_checkpoint_names(listed), vec!["one", "two"]);
    }

    #[test]
    fn a_device_file_lives_where_zerofs_looks_for_an_export() {
        let root = tempfile::tempdir().unwrap();
        let volume_id = VolumeId::parse("vol-1").unwrap();
        assert_eq!(
            filesystem(root.path()).device_file_for(&volume_id),
            root.path().join("mnt/.nbd/vol-1")
        );
    }

    #[tokio::test]
    async fn a_device_file_grows_to_the_size_asked_for_and_is_never_cut_down() {
        let root = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            RecordingCommandRunner::succeeding(),
        );
        let volume_id = VolumeId::parse("vol-1").unwrap();
        assert_eq!(volumes.ensure_device_file(&volume_id, 1024).unwrap(), 1024);
        assert_eq!(volumes.ensure_device_file(&volume_id, 1025).unwrap(), 1536);
        assert_eq!(
            volumes.ensure_device_file(&volume_id, 512).unwrap_err(),
            VolumeError::ShrinkRefused {
                current: 1536,
                requested: 512
            }
        );
    }

    #[tokio::test]
    async fn a_teardown_flushes_before_it_removes_anything() {
        let root = tempfile::tempdir().unwrap();
        let commands = RecordingCommandRunner::succeeding();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands.clone(),
        );
        let volume_id = VolumeId::parse("vol-1").unwrap();
        volumes.ensure_device_file(&volume_id, 1024).unwrap();
        volumes.teardown(&volume_id, &app_id()).await.unwrap();

        let asked: Vec<Vec<String>> = commands.calls().into_iter().map(|call| call.command).collect();
        assert_eq!(
            asked[0],
            vec![
                "/opt/nibrun/bin/zerofs/zerofs".to_string(),
                "flush".into(),
                "-c".into(),
                root.path().join("zerofs.toml").display().to_string(),
            ]
        );
        assert!(!filesystem(root.path()).device_file_for(&volume_id).exists());
    }

    #[tokio::test]
    async fn nothing_here_ever_runs_zerofs_as_a_second_writer() {
        let root = tempfile::tempdir().unwrap();
        let commands = RecordingCommandRunner::succeeding();
        let allocator = Arc::new(Mutex::new(SlotAllocator::empty()));
        let volumes = ZerofsVolumes::new(filesystem(root.path()), allocator, commands.clone());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        volumes.ensure_device_file(&volume_id, 1024).unwrap();
        let _ = volumes.flush().await;
        let _ = volumes
            .create_checkpoint(&crate::test_support::checkpoint_id())
            .await;
        let _ = volumes.observe_checkpoints().await;
        let _ = volumes
            .delete_checkpoint(&crate::test_support::checkpoint_id())
            .await;
        let _ = volumes.teardown(&volume_id, &app_id()).await;

        for call in commands.calls() {
            let writes = call.command.contains(&"run".to_string())
                && !call.command.contains(&"--checkpoint".to_string());
            assert!(
                !writes,
                "a second writer would lose a tenant's data: {:?}",
                call.command
            );
        }
    }

    #[tokio::test]
    async fn a_volume_this_host_does_not_hold_is_said_so_rather_than_attached() {
        let root = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            RecordingCommandRunner::succeeding(),
        );
        let volume_id = VolumeId::parse("vol-nowhere").unwrap();
        assert_eq!(
            volumes.attach(&volume_id, &app_id()).await.unwrap_err(),
            VolumeError::NotHere {
                volume_id: volume_id.clone()
            }
        );
    }

    #[tokio::test]
    async fn a_mount_that_will_not_list_reports_no_volumes() {
        let root = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            RecordingCommandRunner::succeeding(),
        );
        assert!(volumes.observe(&Default::default()).await.is_empty());
    }

    #[tokio::test]
    async fn what_zerofs_was_promised_is_held_back_from_both_memory_and_disk() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("zerofs.toml"),
            "[cache]\ndisk_size_gb = 64\nmemory_size_gb = 2\n",
        )
        .unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            RecordingCommandRunner::succeeding(),
        );
        let reserved = volumes.reserved_cache();
        assert_eq!(reserved.disk_bytes, 64 * 1_073_741_824);
        assert_eq!(reserved.memory_mib(), 2048);
    }

    #[tokio::test]
    async fn a_cache_that_cannot_be_read_is_assumed_rather_than_treated_as_nothing() {
        let root = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            RecordingCommandRunner::succeeding(),
        );
        assert_eq!(volumes.reserved_cache().memory_mib(), 2048);
        assert_eq!(volumes.reserved_cache().disk_bytes, ASSUMED_CACHE_BYTES);
    }

    #[tokio::test]
    async fn a_device_is_attached_first_and_never_formatted_on_a_guess() {
        let root = tempfile::tempdir().unwrap();
        let commands = RecordingCommandRunner::succeeding();
        let sysfs = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands.clone(),
        )
        .with_devices(NbdDevices::with_sysfs(
            sysfs.path().to_path_buf(),
            commands.clone(),
        ));

        let desired = desired_volume(|volume| volume.size_bytes = 268_435_456);
        assert_eq!(
            volumes.provision(&desired).await.unwrap_err(),
            VolumeError::SuperblockUnreadable {
                device_path: "/dev/nbd0".to_string()
            }
        );

        let asked: Vec<Vec<String>> = commands.calls().into_iter().map(|call| call.command).collect();
        assert_eq!(
            asked.len(),
            1,
            "only the attach, and nothing that writes: {asked:?}"
        );
        assert_eq!(asked[0][0], "nbd-client");
        assert_eq!(
            device_file_size(&filesystem(root.path()).device_file_for(&desired.volume_id)),
            Some(268_435_456)
        );
    }
}
