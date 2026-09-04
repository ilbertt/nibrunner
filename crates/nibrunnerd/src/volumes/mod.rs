//! An app's disk. One trait, because what a volume is made of is the one thing about this host
//! that is meant to change: a sparse file works on any Linux box, and blocks in an object store
//! are what makes a volume survive the machine.

pub mod local_file;
pub mod nbd;
pub mod zerofs;

use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{AppId, CheckpointId, DesiredVolume, ObjectKey, VolumeId};

/// Firecracker computes sectors as `size >> 9`, and leaves an unaligned tail invisible to the
/// guest.
pub const SECTOR_SIZE_BYTES: u64 = 512;

pub fn align_to_sector(size_bytes: u64) -> u64 {
    size_bytes.div_ceil(SECTOR_SIZE_BYTES) * SECTOR_SIZE_BYTES
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VolumeError {
    /// Growing preserves the data and shrinking discards everything past the new size, so a
    /// smaller desired size is refused: truncating a tenant's filesystem is not a way to discover
    /// a bug.
    #[error("a volume of {current} bytes cannot be resized down to {requested}")]
    ShrinkRefused { current: u64, requested: u64 },
    #[error("{device_path} did not answer a read of its superblock")]
    SuperblockUnreadable { device_path: String },
    #[error("the volume could not be made ready: {0}")]
    Unusable(String),
    #[error("this host does not serve {volume_id}")]
    NotHere { volume_id: VolumeId },
    #[error("{what} cannot be checkpointed")]
    NoCheckpoints { what: &'static str },
}

impl VolumeError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// What a backend hands back once a volume is somewhere a guest can be pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedVolume {
    pub volume_id: VolumeId,
    /// What Firecracker is given as the data drive.
    pub device_path: String,
    pub size_bytes: u64,
    pub storage_prefix: ObjectKey,
}

/// What this host is holding for one volume, as an observation rather than a memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedBacking {
    pub volume_id: VolumeId,
    pub size_bytes: u64,
    /// Whether the device *works*, not whether something claims it: this is what the planner
    /// reads to decide a volume needs nothing done to it, so a backing that lies here is one
    /// nothing ever repairs.
    pub attached: bool,
    pub device_path: Option<String>,
    pub storage_prefix: ObjectKey,
}

/// What a storage service on this host has been promised: disk and memory it has not taken yet
/// rather than what it is holding now. It grows into both lazily, so free space on a fresh host is
/// space already spoken for, and anything else helping itself to either has to hold this much back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheReservation {
    pub disk_bytes: u64,
    pub memory_bytes: u64,
}

impl CacheReservation {
    pub fn memory_mib(self) -> u64 {
        self.memory_bytes.div_ceil(1_048_576)
    }
}

#[async_trait]
pub trait VolumeBackend: Send + Sync {
    /// A volume of at least this size, formatted if it has never been, and reachable at a device
    /// path. Idempotent: a converged host runs this to no effect.
    async fn provision(&self, desired: &DesiredVolume) -> Result<AttachedVolume, VolumeError>;

    /// The device behind a volume that already exists, put back where a guest can reach it. What
    /// a host does on the way up for every volume it still holds.
    ///
    /// The app is passed rather than derived, here and below. A backend that keeps blocks in an
    /// object store reaches them on the NBD minor the app's slot names, and the only party that
    /// knows which app a volume belongs to is the one holding the document — a backend that
    /// guessed would attach one tenant's disk under another's name.
    async fn attach(&self, volume_id: &VolumeId, app_id: &AppId) -> Result<AttachedVolume, VolumeError>;

    async fn detach(&self, volume_id: &VolumeId, app_id: &AppId) -> Result<(), VolumeError>;

    /// The only path that destroys tenant data, and it runs only for an explicit `absent`.
    async fn teardown(&self, volume_id: &VolumeId, app_id: &AppId) -> Result<(), VolumeError>;

    /// Everything acknowledged, durable. The durability point every stop and every sleep is taken
    /// at: a backend whose writes are already durable answers immediately, and one that batches
    /// has to be asked.
    async fn flush(&self) -> Result<(), VolumeError>;

    /// A pinned, non-advancing view, for a reader that must not see the tenant move. A backend
    /// that cannot cut one says so rather than pretending: a checkpoint reported ready that
    /// nothing pinned is a reader that sees the tenant move underneath it.
    async fn create_checkpoint(&self, checkpoint_id: &CheckpointId) -> Result<(), VolumeError>;

    async fn delete_checkpoint(&self, checkpoint_id: &CheckpointId) -> Result<(), VolumeError>;

    /// The checkpoints this host is actually holding, by name. Which volume each was cut for is
    /// not in the answer, for the same reason a volume's owner is not: the store knows names, and
    /// only the document knows what they are for.
    async fn observe_checkpoints(&self) -> Vec<CheckpointId>;

    /// What this host is actually holding, which is what a restarted daemon converges against.
    /// The owners come from the document for the same reason they are passed to `attach`.
    async fn observe(&self, owners: &BTreeMap<VolumeId, AppId>) -> Vec<ObservedBacking>;

    /// What this backend's storage service has been promised. Asked of the backend rather than
    /// read from the config by whoever needs it, because the two callers — the memory a guest may
    /// be given, and the disk a snapshot may go on — have to agree by construction: a host that
    /// held memory back by one measure and disk by another would refuse and over-commit at once.
    fn reserved_cache(&self) -> CacheReservation {
        CacheReservation::default()
    }
}

/// What `mkfs.ext4` writes at offset 1080 of a filesystem it made. Comparing a constant, not
/// parsing a filesystem: the host must never let its kernel interpret tenant-controlled metadata,
/// and this is the only thing distinguishing a blank device.
pub const SUPERBLOCK_MAGIC_OFFSET: u64 = 1080;
const EXT_MAGIC: u16 = 0xef53;

pub fn has_ext_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && u16::from_le_bytes([bytes[0], bytes[1]]) == EXT_MAGIC
}

pub const FILESYSTEM_LABEL: &str = "nibrun-data";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_is_rounded_up_to_a_sector_so_no_tail_is_invisible_to_the_guest() {
        assert_eq!(align_to_sector(0), 0);
        assert_eq!(align_to_sector(1), SECTOR_SIZE_BYTES);
        assert_eq!(align_to_sector(SECTOR_SIZE_BYTES), SECTOR_SIZE_BYTES);
        assert_eq!(align_to_sector(SECTOR_SIZE_BYTES + 1), SECTOR_SIZE_BYTES * 2);
    }

    #[test]
    fn only_the_ext_magic_counts_as_a_formatted_filesystem() {
        assert!(has_ext_magic(&0xef53u16.to_le_bytes()));
        assert!(!has_ext_magic(&0u16.to_le_bytes()));
        assert!(!has_ext_magic(&[0x53]));
    }
}
