use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DesiredVolume, ObjectKey, VolumeId};
use tokio::sync::Mutex;

use crate::adapters::net::allocator::SlotAllocator;
use crate::adapters::volumes::initial_contents::{ContentsStaging, StagedRoot};
use crate::adapters::volumes::nbd::{NbdDevices, NbdTarget};
use crate::adapters::volumes::{
    align_to_sector, format_request, has_ext_magic, AttachedVolume, CacheReservation, ObservedBacking,
    VolumeBackend, VolumeError, SUPERBLOCK_MAGIC_OFFSET,
};
use crate::ports::{CommandRequest, CommandRunner, CommandRunnerExt};

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
    contents: ContentsStaging,
}

impl ZerofsVolumes {
    pub fn new(
        filesystem: ZerofsFilesystem,
        allocator: Arc<Mutex<SlotAllocator>>,
        commands: Arc<dyn CommandRunner>,
        contents: ContentsStaging,
    ) -> Self {
        Self {
            filesystem,
            devices: NbdDevices::new(commands.clone()),
            allocator,
            commands,
            contents,
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

    async fn format(&self, device_path: &str, contents: Option<&StagedRoot>) -> Result<(), VolumeError> {
        self.commands
            .stdout_of(format_request(device_path, contents, false))
            .await
            .map_err(|error| VolumeError::Unusable(error.message()))?;
        Ok(())
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

const LISTING_HEADING: &str = "Name";

fn draws_the_table(character: char) -> bool {
    ('\u{2500}'..='\u{257f}').contains(&character)
}

// The listing is a table drawn in box characters, so a row's name is the first word of its first
// cell and not the first word of the line: that one is the border the row opens with. A row made
// only of border has no cell to read, and the heading names the column rather than a checkpoint.
pub fn parse_checkpoint_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            line.split(draws_the_table)
                .find_map(|cell| cell.split_whitespace().next())
        })
        .filter(|name| *name != LISTING_HEADING)
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
        if slot.slot >= nft_render::NBD_SLOT_LIMIT {
            return Err(VolumeError::Unusable(format!(
                "slot {} needs /dev/nbd{}, and this backend addresses one minor per slot: \
                 load the nbd module with nbds_max above {}, or keep this host under that many apps",
                slot.slot,
                slot.slot,
                nft_render::NBD_SLOT_LIMIT
            )));
        }
        let socket_path = self.filesystem.nbd_socket_path.display().to_string();
        let target = NbdTarget {
            socket_path: &socket_path,
            device_path: &slot.nbd_device_path,
            volume_id: &desired.volume_id,
        };
        if !self.devices.is_usable(&slot.nbd_device_path).await {
            self.devices.reattach(&target).await?;
        }
        if !self.is_formatted(&slot.nbd_device_path).await? {
            let contents = self.contents.stage_for(desired).await?;
            self.format(&slot.nbd_device_path, contents.as_ref()).await?;
            tracing::info!(
                volume_id = %desired.volume_id,
                device = %slot.nbd_device_path,
                seeded = contents.is_some(),
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
        // A checkpoint is named after the export that owns it, so one already under this name is
        // the last attempt at this same export and is what a retry is retrying. Creating over it
        // is refused rather than ignored, which would make the second attempt fail where the
        // first merely did not finish.
        if self.observe_checkpoints().await.contains(checkpoint_id) {
            self.delete_checkpoint(checkpoint_id).await?;
        }
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
    use crate::adapters::volumes::initial_contents::tests as seed;
    use crate::test_support::mocks;
    use crate::test_support::{app_id, desired_volume};

    fn staging(root: &Path) -> ContentsStaging {
        seed::staging(&root.join("staging"), seed::archive())
    }

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
    fn the_names_are_read_out_of_the_table_the_cli_actually_prints() {
        // Copied from a host: every row opens with a border, so a reading that takes the first
        // word of the line comes back holding box characters and no checkpoint is ever seen.
        let listed = concat!(
            "┌──────────────┬──────────────────────────────────────┬─────────────────────┐\n",
            "│ Name         ┆ ID                                   ┆ Created At          │\n",
            "╞══════════════╪══════════════════════════════════════╪═════════════════════╡\n",
            "│ export-exp-g ┆ 4eaaf567-f923-4a67-b3ff-388487cd884e ┆ 2026-09-09 16:15:37 │\n",
            "├╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤\n",
            "│ export-exp-b ┆ 426aee74-6f6c-4cb4-9e46-1ec50abdf569 ┆ 2026-09-09 16:02:34 │\n",
            "└──────────────┴──────────────────────────────────────┴─────────────────────┘\n",
        );
        assert_eq!(
            parse_checkpoint_names(listed),
            vec!["export-exp-g", "export-exp-b"]
        );
    }

    #[test]
    fn a_table_holding_no_rows_names_no_checkpoints() {
        let listed = concat!(
            "┌──────┬────┐\n",
            "│ Name ┆ ID │\n",
            "╞══════╪════╡\n",
            "└──────┴────┘\n",
        );
        assert!(parse_checkpoint_names(listed).is_empty(), "{listed}");
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
            mocks::commands_succeeding().0,
            staging(root.path()),
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
        let (commands, log) = mocks::commands_succeeding();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands.clone(),
            staging(root.path()),
        );
        let volume_id = VolumeId::parse("vol-1").unwrap();
        volumes.ensure_device_file(&volume_id, 1024).unwrap();
        volumes.teardown(&volume_id, &app_id()).await.unwrap();

        let asked = log.commands();
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
        let (commands, log) = mocks::commands_succeeding();
        let allocator = Arc::new(Mutex::new(SlotAllocator::empty()));
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            allocator,
            commands.clone(),
            staging(root.path()),
        );
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

        for call in log.calls() {
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
            mocks::commands_succeeding().0,
            staging(root.path()),
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
            mocks::commands_succeeding().0,
            staging(root.path()),
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
            mocks::commands_succeeding().0,
            staging(root.path()),
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
            mocks::commands_succeeding().0,
            staging(root.path()),
        );
        assert_eq!(volumes.reserved_cache().memory_mib(), 2048);
        assert_eq!(volumes.reserved_cache().disk_bytes, ASSUMED_CACHE_BYTES);
    }

    #[tokio::test]
    async fn a_device_is_attached_first_and_never_formatted_on_a_guess() {
        let root = tempfile::tempdir().unwrap();
        let (commands, log) = mocks::commands_succeeding();
        let sysfs = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands.clone(),
            staging(root.path()),
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

        let asked = log.commands();
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

    fn volumes_with_sysfs(
        root: &Path,
        sysfs: &Path,
        commands: Arc<crate::ports::MockCommandRunner>,
    ) -> ZerofsVolumes {
        ZerofsVolumes::new(
            filesystem(root),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands.clone(),
            staging(root),
        )
        .with_devices(NbdDevices::with_sysfs(sysfs.to_path_buf(), commands))
    }

    #[test]
    fn every_export_this_host_serves_lives_under_one_directory_of_the_mount() {
        let root = tempfile::tempdir().unwrap();
        let filesystem = filesystem(root.path());
        assert_eq!(filesystem.nbd_directory(), root.path().join("mnt/.nbd"));
        assert!(filesystem
            .device_file_for(&VolumeId::parse("vol-1").unwrap())
            .starts_with(filesystem.nbd_directory()));
    }

    #[tokio::test]
    async fn an_app_that_holds_no_slot_has_no_device_to_be_detached_from() {
        let root = tempfile::tempdir().unwrap();
        let (commands, log) = mocks::commands_succeeding();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands,
            staging(root.path()),
        );
        volumes
            .detach(&VolumeId::parse("vol-1").unwrap(), &app_id())
            .await
            .unwrap();
        assert!(log.calls().is_empty());
    }

    #[tokio::test]
    async fn a_volume_whose_app_holds_no_slot_has_nowhere_to_be_attached() {
        let root = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            mocks::commands_succeeding().0,
            staging(root.path()),
        );
        let volume_id = VolumeId::parse("vol-1").unwrap();
        volumes.ensure_device_file(&volume_id, 1024).unwrap();
        assert_eq!(
            volumes.attach(&volume_id, &app_id()).await.unwrap_err(),
            VolumeError::NotHere {
                volume_id: volume_id.clone()
            }
        );
    }

    #[tokio::test]
    async fn what_the_mount_holds_is_listed_and_what_is_not_a_volume_is_passed_over() {
        let root = tempfile::tempdir().unwrap();
        let sysfs = tempfile::tempdir().unwrap();
        let (commands, _) = mocks::commands_succeeding();
        let volumes = volumes_with_sysfs(root.path(), sysfs.path(), commands);
        let held = VolumeId::parse("vol-1").unwrap();
        volumes.ensure_device_file(&held, 4096).unwrap();
        std::fs::write(volumes.filesystem.nbd_directory().join("not a volume"), b"").unwrap();
        std::fs::create_dir_all(volumes.filesystem.nbd_directory().join("vol-2")).unwrap();

        let observed = volumes.observe(&Default::default()).await;
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].volume_id, held);
        assert_eq!(observed[0].size_bytes, 4096);
        assert_eq!(observed[0].device_path, None);
        assert!(!observed[0].attached, "a volume no app holds is not attached");
        assert_eq!(observed[0].storage_prefix, ObjectKey::parse("volumes").unwrap());
    }

    #[tokio::test]
    async fn a_volume_an_app_holds_is_observed_on_the_device_that_app_was_given() {
        let root = tempfile::tempdir().unwrap();
        let (commands, _) = mocks::commands_succeeding();
        let allocator = Arc::new(Mutex::new(SlotAllocator::empty()));
        let slot = allocator.lock().await.allocate(&app_id()).unwrap();
        let sysfs = tempfile::tempdir().unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            allocator,
            commands.clone(),
            staging(root.path()),
        )
        .with_devices(NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands));
        let held = VolumeId::parse("vol-1").unwrap();
        volumes.ensure_device_file(&held, 4096).unwrap();

        let owners = std::collections::BTreeMap::from([(held.clone(), app_id())]);
        let observed = volumes.observe(&owners).await;
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].device_path.as_deref(),
            Some(slot.nbd_device_path.as_str())
        );
        assert!(!observed[0].attached, "nothing in sysfs holds the device");
    }

    #[tokio::test]
    async fn a_checkpoint_zerofs_would_not_take_is_a_refusal_rather_than_a_silent_success() {
        let root = tempfile::tempdir().unwrap();
        let (commands, _) = mocks::commands_answering(|request| {
            Err(crate::ports::CommandError::Unstartable {
                executable: request.executable().to_string(),
                reason: "no such file".into(),
            })
        });
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands,
            staging(root.path()),
        );
        for refused in [
            volumes
                .create_checkpoint(&crate::test_support::checkpoint_id())
                .await,
            volumes
                .delete_checkpoint(&crate::test_support::checkpoint_id())
                .await,
            volumes.flush().await,
        ] {
            let error = refused.unwrap_err();
            assert!(matches!(error, VolumeError::Unusable(_)), "{error}");
        }
        assert!(volumes.observe_checkpoints().await.is_empty());
    }

    #[tokio::test]
    async fn every_checkpoint_command_names_the_checkpoint_and_the_configuration() {
        let root = tempfile::tempdir().unwrap();
        let (commands, log) = mocks::commands_answering(|request| {
            let listing = request.command.contains(&"list".to_string());
            Ok(crate::ports::CommandResult::with_stdout(if listing {
                "chk-1  2026-09-04\nnot.a.checkpoint  2026-09-05\n"
            } else {
                ""
            }))
        });
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands,
            staging(root.path()),
        );
        volumes
            .create_checkpoint(&crate::test_support::checkpoint_id())
            .await
            .unwrap();
        volumes
            .delete_checkpoint(&crate::test_support::checkpoint_id())
            .await
            .unwrap();
        let listed = volumes.observe_checkpoints().await;
        assert_eq!(listed, vec![crate::test_support::checkpoint_id()]);

        let asked = log.commands();
        // Creating looks first, and this store already holds the name, so the last attempt at it
        // goes before the new one is cut.
        assert_eq!(asked[0][1..3], ["checkpoint", "list"]);
        assert_eq!(asked[1][1..4], ["checkpoint", "delete", "chk-1"]);
        assert_eq!(asked[2][1..4], ["checkpoint", "create", "chk-1"]);
        assert_eq!(asked[3][1..4], ["checkpoint", "delete", "chk-1"]);
        assert_eq!(asked[4][1..3], ["checkpoint", "list"]);
        for call in asked {
            assert_eq!(call[0], "/opt/nibrun/bin/zerofs/zerofs");
            assert_eq!(call[call.len() - 2], "-c");
        }
    }

    #[tokio::test]
    async fn a_device_file_that_will_not_be_made_is_named_rather_than_provisioned_around() {
        let root = tempfile::tempdir().unwrap();
        let filesystem = filesystem(root.path());
        std::fs::create_dir_all(&filesystem.mount_path).unwrap();
        std::fs::write(filesystem.nbd_directory(), b"a file, not a directory").unwrap();
        let volumes = ZerofsVolumes::new(
            filesystem,
            Arc::new(Mutex::new(SlotAllocator::empty())),
            mocks::commands_succeeding().0,
            staging(root.path()),
        );
        let error = volumes
            .ensure_device_file(&VolumeId::parse("vol-1").unwrap(), 1024)
            .unwrap_err();
        assert!(matches!(error, VolumeError::Unusable(_)), "{error}");
    }

    #[test]
    fn a_cache_size_written_as_a_fraction_is_read_down_to_whole_gigabytes() {
        assert_eq!(
            cache_gigabytes("[cache]\ndisk_size_gb = 1.5\n", "disk_size_gb"),
            Some(1)
        );
        assert_eq!(
            cache_gigabytes("[cache]\ndisk_size_gb = 0.5\n", "disk_size_gb"),
            None
        );
        assert_eq!(
            cache_gigabytes("[cache]\ndisk_size_gb = -1.5\n", "disk_size_gb"),
            None
        );
        assert_eq!(
            cache_gigabytes("[storage]\ndisk_size_gb = 4\n", "disk_size_gb"),
            None
        );
    }

    #[test]
    fn a_listing_with_nothing_in_it_names_no_checkpoints() {
        assert!(parse_checkpoint_names("").is_empty());
        assert!(parse_checkpoint_names("\n   \n\t\n").is_empty());
        assert_eq!(parse_checkpoint_names("  leading   space\n"), vec!["leading"]);
    }

    #[test]
    fn a_superblock_read_off_the_end_of_a_short_file_is_not_read_as_a_filesystem() {
        let directory = tempfile::tempdir().unwrap();
        let short = directory.path().join("short");
        std::fs::write(&short, b"tiny").unwrap();
        assert_eq!(read_superblock_magic(&short.display().to_string()), Some([0, 0]));
        assert_eq!(
            read_superblock_magic(&directory.path().join("absent").display().to_string()),
            None
        );

        let formatted = directory.path().join("formatted");
        let mut image = vec![0u8; SUPERBLOCK_MAGIC_OFFSET as usize + 2];
        image[SUPERBLOCK_MAGIC_OFFSET as usize..].copy_from_slice(&0xef53u16.to_le_bytes());
        std::fs::write(&formatted, &image).unwrap();
        assert_eq!(
            read_superblock_magic(&formatted.display().to_string()),
            Some(0xef53u16.to_le_bytes())
        );
    }

    #[tokio::test]
    async fn a_device_nothing_can_be_read_from_is_never_taken_as_unformatted_and_written_over() {
        let root = tempfile::tempdir().unwrap();
        let (commands, log) = mocks::commands_succeeding();
        let volumes = ZerofsVolumes::new(
            filesystem(root.path()),
            Arc::new(Mutex::new(SlotAllocator::empty())),
            commands,
            staging(root.path()),
        );
        let error = volumes
            .is_formatted("/dev/nbd-that-is-not-there")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            VolumeError::SuperblockUnreadable {
                device_path: "/dev/nbd-that-is-not-there".to_string()
            }
        );
        assert!(log.calls().is_empty(), "reading a superblock runs no tool");
    }
}
