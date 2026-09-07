pub mod local_file;
pub mod nbd;
pub mod zerofs;

use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{AppId, CheckpointId, DesiredVolume, ObjectKey, VolumeId};

pub const SECTOR_SIZE_BYTES: u64 = 512;

pub fn align_to_sector(size_bytes: u64) -> u64 {
    size_bytes.div_ceil(SECTOR_SIZE_BYTES) * SECTOR_SIZE_BYTES
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VolumeError {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedVolume {
    pub volume_id: VolumeId,
    pub device_path: String,
    pub size_bytes: u64,
    pub storage_prefix: ObjectKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedBacking {
    pub volume_id: VolumeId,
    pub size_bytes: u64,
    pub attached: bool,
    pub device_path: Option<String>,
    pub storage_prefix: ObjectKey,
}

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

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait VolumeBackend: Send + Sync {
    async fn provision(&self, desired: &DesiredVolume) -> Result<AttachedVolume, VolumeError>;

    async fn attach(&self, volume_id: &VolumeId, app_id: &AppId) -> Result<AttachedVolume, VolumeError>;

    async fn detach(&self, volume_id: &VolumeId, app_id: &AppId) -> Result<(), VolumeError>;

    async fn teardown(&self, volume_id: &VolumeId, app_id: &AppId) -> Result<(), VolumeError>;

    async fn flush(&self) -> Result<(), VolumeError>;

    async fn create_checkpoint(&self, checkpoint_id: &CheckpointId) -> Result<(), VolumeError>;

    async fn delete_checkpoint(&self, checkpoint_id: &CheckpointId) -> Result<(), VolumeError>;

    async fn observe_checkpoints(&self) -> Vec<CheckpointId>;

    async fn observe(&self, owners: &BTreeMap<VolumeId, AppId>) -> Vec<ObservedBacking>;

    fn reserved_cache(&self) -> CacheReservation {
        CacheReservation::default()
    }
}

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
