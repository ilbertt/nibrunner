//! A volume as a file inside a ZeroFS filesystem, reached by the guest over NBD.
//!
//! Ported from `apps/agent/src/lib/volumes/zerofs.ts`, `topology.ts`, `device-file.ts` and
//! `services/volume-manager.service.ts`. This is the backend that makes a volume outlive the
//! machine it was written on: the blocks are in an object store, and what the host holds is a
//! cache and a mount.
//!
//! **ZeroFS is a long-running service this daemon does not own, and never starts.** Restarting it
//! stalls every attached microVM at once and drops whatever was acknowledged but unflushed, so
//! everything here talks to it over its admin CLI and nothing here launches one.
//!
//! The invariant that makes that a rule rather than a preference: **exactly one read-write
//! `zerofs run` per storage prefix, fleet-wide.** A second writer is fenced by SlateDB's epoch
//! only after a window of acknowledging writes that are then discarded — so a host that started
//! its own would lose a tenant's data rather than fail to start. Whatever supervises the host is
//! the lock, and a single-instance unit is how it is held. A checkpoint server is not a second
//! writer: it is opened `--checkpoint`, which ZeroFS refuses to open read-write at all, against a
//! pinned manifest that no longer advances, so it takes no epoch and can acknowledge nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DesiredVolume, ObjectKey, VolumeId};
use tokio::sync::Mutex;

use crate::net::allocator::SlotAllocator;
use crate::services::{CommandRequest, CommandRunner};
use crate::volumes::nbd::{NbdDevices, NbdTarget};
use crate::volumes::{
    align_to_sector, has_ext_magic, AttachedVolume, CacheReservation, ObservedBacking, VolumeBackend,
    VolumeError, FILESYSTEM_LABEL, SUPERBLOCK_MAGIC_OFFSET,
};

/// ZeroFS resolves every NBD export by looking this directory up on each negotiation.
pub const NBD_DIRECTORY: &str = ".nbd";

/// ZeroFS's `gb` read as GiB, the larger of the two it could mean. The reading is only ever used
/// to hold disk back for it, and reserving five gigabytes too many costs a cold boot where
/// reserving five too few costs the cache every app on the host runs from.
const BYTES_PER_CONFIGURED_GB: u64 = 1_073_741_824;

/// The two `[cache]` sizes this daemon holds back for, spelled as ZeroFS spells them.
const CACHE_DISK_SETTING: &str = "disk_size_gb";
const CACHE_MEMORY_SETTING: &str = "memory_size_gb";

/// What ZeroFS is assumed to want when its own config cannot be read, which is the number that
/// config holds today.
///
/// Guessed rather than raised into a failure, because the two ways of being wrong are not
/// symmetrical: a host that reports no capacity is one nothing is ever placed on, where a host
/// that reports memory it does not have kills a tenant — and, ZeroFS being the disk every app on
/// the host runs through, most likely several of them.
const ASSUMED_CACHE_BYTES: u64 = 2048 * 1_048_576;

/// Where this host's ZeroFS is, and where what it serves can be reached.
///
/// One ZeroFS per host, so every volume placed here shares one prefix — one process, one cache,
/// one S3 client, and the only shape whose cost does not scale with tenant count. What it costs is
/// that the restore property is per host rather than per app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZerofsFilesystem {
    pub storage_prefix: ObjectKey,
    /// The host's own mount, where `.nbd/<volume-id>` is created and sized. What that file holds
    /// is an image this host never asks its kernel to interpret.
    pub mount_path: PathBuf,
    pub nbd_socket_path: PathBuf,
    /// One directory per checkpoint server, each with an NBD socket of its own. Separate from
    /// `nbd_socket_path` because a checkpoint is served by a second process, and sharing the live
    /// server's path would be two listeners fighting over one address.
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

    /// `zerofs <args> -c <config>`, which is the whole of how this daemon talks to the service.
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

    /// The device a volume's app was given. A volume with no slot is one this host has no record
    /// of placing, which is a different thing from one whose device is broken.
    async fn device_for(&self, app_id: &protocol::AppId) -> Option<String> {
        self.allocator
            .lock()
            .await
            .lookup(app_id)
            .map(|slot| slot.nbd_device_path)
    }

    /// Growing preserves the data and shrinking discards everything past the new size, so a
    /// smaller desired size is refused: truncating a tenant's filesystem is not a way to discover
    /// a bug.
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

    /// Comparing a constant, not parsing a filesystem: the host must never let its kernel
    /// interpret tenant-controlled metadata, and this is the only thing distinguishing a blank
    /// device.
    ///
    /// Raised rather than guessed when the device will not answer. Both guesses are wrong in a way
    /// this is not allowed to be: unformatted destroys a tenant's filesystem, and formatted
    /// reports a volume ready that nothing can read. A failure here is a volume reported failed,
    /// which is the only one of the three an operator can act on.
    async fn is_formatted(&self, device_path: &str) -> Result<bool, VolumeError> {
        let path = device_path.to_string();
        let unreadable = || VolumeError::SuperblockUnreadable {
            device_path: device_path.to_string(),
        };
        // On the same blocking thread and the same deadline as the liveness probe, and for the
        // same reason: this opens a block device, and one whose NBD server has gone answers
        // neither the open nor the read.
        let read = tokio::task::spawn_blocking(move || read_superblock_magic(&path));
        match tokio::time::timeout(SUPERBLOCK_READ_TIMEOUT, read).await {
            Ok(Ok(Some(magic))) => Ok(has_ext_magic(&magic)),
            _ => Err(unreadable()),
        }
    }

    /// Deliberately the defaults.
    ///
    /// Formatting an 8 GiB volume measures 0.10s against a real ZeroFS, and ~0.09s of that is
    /// recoverable only by `lazy_journal_init` — the one extended option that narrows what
    /// survives an unclean shutdown. A log-structured store with compression is why: a fresh
    /// filesystem is almost entirely zeroes, so what reaches S3 is a few hundred KiB whatever
    /// `mke2fs` is asked to write. There is no time here worth buying.
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

    /// What ZeroFS is entitled to, read out of the file it is itself started with rather than
    /// written down a second time here. It grows into both numbers lazily, so a disk or a host
    /// that looks empty today is one whose free space is already spoken for — and anything else
    /// helping itself to either has to hold this much back rather than what ZeroFS happens to be
    /// holding now.
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

/// The same ceiling the liveness probe carries, for the same reason.
const SUPERBLOCK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn device_file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|info| info.is_file())
        .map(|info| info.len())
}

/// `None` where the device would not answer, which is the case that must not be guessed at.
fn read_superblock_magic(device_path: &str) -> Option<[u8; 2]> {
    use std::io::{Read, Seek, SeekFrom};
    let mut device = std::fs::File::open(device_path).ok()?;
    device.seek(SeekFrom::Start(SUPERBLOCK_MAGIC_OFFSET)).ok()?;
    let mut magic = [0u8; 2];
    match device.read_exact(&mut magic) {
        Ok(()) => Some(magic),
        // Shorter than a superblock is a device nothing has formatted, not one that refused to
        // answer: a blank device reads as zeroes, which is not the magic.
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Some([0, 0]),
        Err(_) => None,
    }
}

/// The configured number, or nothing at all: a value this cannot read is not one to guess at.
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

/// Names one per line, whatever else the line carries.
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
        // Only where the device does not already work. A reattach on a healthy device is a tenant
        // whose disk goes away for the length of one, for no reason.
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

    /// The only path that destroys tenant data, and it runs only for an explicit `absent`. The
    /// flush first, so the detach happens at a durability point rather than dropping whatever the
    /// periodic flush had not yet uploaded.
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

    /// Under `ignore_fsync` the guest's own flushes are a no-op, so this is the whole of what
    /// stands between a microVM going down — stopped or asleep — and the loss of everything since
    /// the last periodic flush.
    async fn flush(&self) -> Result<(), VolumeError> {
        self.admin(&["flush"]).await.map(|_| ())
    }

    /// Named by the document rather than by this host. A checkpoint is of the whole filesystem —
    /// one ZeroFS per host, every volume in it — so the name is the only thing that says which one
    /// was asked for, and an agent killed between cutting one and reporting it comes back to a
    /// list of names it recognises rather than to a recovery mode.
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

    /// An empty list where ZeroFS would not answer, which reads as a host holding no checkpoints.
    /// That is the safe way round: a name this host cannot see is one the pass will try to cut
    /// again, where a name it invents is one nothing will ever delete.
    async fn observe_checkpoints(&self) -> Vec<protocol::CheckpointId> {
        let Ok(listed) = self.admin(&["checkpoint", "list"]).await else {
            return Vec::new();
        };
        parse_checkpoint_names(&listed)
            .into_iter()
            .filter_map(|name| protocol::CheckpointId::parse(&name).ok())
            .collect()
    }

    /// A reading that could not be taken is the assumed number rather than none, and it is said
    /// out loud: a host whose ZeroFS config moved would otherwise silently promise a tenant memory
    /// the cache is going to take back.
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
            // A mount that will not list is a ZeroFS that is not there, and reporting no volumes
            // is what a host whose filesystems live elsewhere looks like. Said out loud, because
            // the two are indistinguishable to whatever reads the report.
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
            // A device file with no app is one this daemon has lost its record of. It is reported
            // with no device rather than under a guessed app: what reads these decides on the
            // strength of them that a tenant's filesystem is gone.
            let device_path = match owners.get(&volume_id) {
                Some(app_id) => self.device_for(app_id).await,
                None => None,
            };
            // Whether the device *works*, not whether it has a client: this is what the planner
            // reads to decide a volume needs nothing done to it, so a backing that lies here is
            // one nothing ever repairs.
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
    use crate::services::RecordingCommandRunner;
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

    /// A value this cannot read is not one to guess at: what the reading is used for is holding
    /// disk back, and a guess of zero is a host that lets something else take ZeroFS's cache.
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
        // Rounded up, because Firecracker computes sectors as `size >> 9`.
        assert_eq!(volumes.ensure_device_file(&volume_id, 1025).unwrap(), 1536);
        assert_eq!(
            volumes.ensure_device_file(&volume_id, 512).unwrap_err(),
            VolumeError::ShrinkRefused {
                current: 1536,
                requested: 512
            }
        );
    }

    /// The flush is the durability point. Tearing down without one drops whatever the periodic
    /// flush had not yet uploaded, on the one path that is allowed to destroy data.
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

    /// This daemon never starts a read-write server, so the only thing it may ever run is the
    /// admin CLI and the tools that attach what the service already exports.
    #[tokio::test]
    async fn nothing_here_ever_runs_zerofs_as_a_server() {
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
        let _ = volumes.teardown(&volume_id, &app_id()).await;

        for call in commands.calls() {
            assert!(
                !call.command.contains(&"run".to_string()),
                "a second writer would be fenced only after acknowledging writes it then discards: {:?}",
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

    /// What a restarted daemon converges against is what ZeroFS is exporting, not what it
    /// remembers — so an unreadable mount reports nothing rather than the last thing it knew.
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

    /// The two callers of this — the memory a guest may be given, and the disk a snapshot may go
    /// on — have to agree by construction, so both come from here.
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

    /// A host that reports memory it does not have kills a tenant, and ZeroFS being the disk every
    /// app runs through, most likely several — so a reading that could not be taken is the assumed
    /// number and never none.
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

    /// The device is attached before anything is written to it, and a device that will not
    /// answer a read of its superblock stops the provision rather than being formatted on a
    /// guess: formatting one that was merely unreachable destroys a tenant's filesystem.
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
        // `/dev/nbd0` is not a device on the machine running this, which is the same shape as one
        // whose server has gone: unreadable, and so not something to format.
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
        // The device file is sized whatever the device did, because that is ZeroFS's export and
        // not the kernel's device: the next pass finds it and attaches again.
        assert_eq!(
            device_file_size(&filesystem(root.path()).device_file_for(&desired.volume_id)),
            Some(268_435_456)
        );
    }
}
