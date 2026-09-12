pub mod initial_contents;
pub mod local_file;
pub mod nbd;
pub mod zerofs;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use protocol::{AppId, CheckpointId, DesiredVolume, ObjectKey, VolumeId};

use crate::adapters::volumes::initial_contents::StagedRoot;
use crate::ports::CommandRequest;

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
    #[error("the initial contents could not be laid out: {0}")]
    ContentsUnusable(String),
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

/// A format that copies contents in is as long as the contents; one that does not is seconds.
const SEEDED_FORMAT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// One ext4 over `device_path`, holding `contents` from the start when there are any. `-F`
/// is for a regular file, which mke2fs otherwise stops to ask about.
pub(crate) fn format_request(
    device_path: &str,
    contents: Option<&StagedRoot>,
    regular_file: bool,
) -> CommandRequest {
    let mut command = vec!["mke2fs", "-q", "-t", "ext4"];
    if regular_file {
        command.push("-F");
    }
    command.extend(["-L", FILESYSTEM_LABEL]);
    let root;
    if let Some(staged) = contents {
        root = staged.path().display().to_string();
        command.extend(["-d", root.as_str()]);
    }
    command.push(device_path);
    let mut request = CommandRequest::new(&command);
    if contents.is_some() {
        request.timeout = SEEDED_FORMAT_TIMEOUT;
    }
    request
}

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

    #[test]
    fn the_magic_is_read_little_endian_so_the_bytes_the_other_way_round_are_not_it() {
        assert!(!has_ext_magic(&0xef53u16.to_be_bytes()));
        assert!(!has_ext_magic(&[]));
        assert!(has_ext_magic(&[0x53, 0xef, 0x00, 0x01]));
    }

    #[test]
    fn a_cache_is_held_back_in_whole_mebibytes_so_none_of_it_is_handed_out_twice() {
        assert_eq!(CacheReservation::default().memory_mib(), 0);
        assert_eq!(
            CacheReservation {
                memory_bytes: 1,
                disk_bytes: 0
            }
            .memory_mib(),
            1
        );
        assert_eq!(
            CacheReservation {
                memory_bytes: 2 * 1_048_576,
                disk_bytes: 0
            }
            .memory_mib(),
            2
        );
        assert_eq!(
            CacheReservation {
                memory_bytes: 2 * 1_048_576 + 1,
                disk_bytes: 0
            }
            .memory_mib(),
            3
        );
    }

    #[test]
    fn every_way_a_volume_can_refuse_reads_as_a_sentence_naming_what_went_wrong() {
        assert_eq!(
            VolumeError::ShrinkRefused {
                current: 2048,
                requested: 1024
            }
            .message(),
            "a volume of 2048 bytes cannot be resized down to 1024"
        );
        assert_eq!(
            VolumeError::SuperblockUnreadable {
                device_path: "/dev/nbd0".into()
            }
            .message(),
            "/dev/nbd0 did not answer a read of its superblock"
        );
        assert_eq!(
            VolumeError::NotHere {
                volume_id: VolumeId::parse("vol-1").unwrap()
            }
            .message(),
            "this host does not serve vol-1"
        );
        assert_eq!(
            VolumeError::NoCheckpoints { what: "a local file" }.message(),
            "a local file cannot be checkpointed"
        );
        assert_eq!(
            VolumeError::Unusable("the device would not open".into()).message(),
            "the volume could not be made ready: the device would not open"
        );
        assert_eq!(
            VolumeError::ContentsUnusable("the archive could not be read".into()).message(),
            "the initial contents could not be laid out: the archive could not be read"
        );
    }

    #[tokio::test]
    async fn a_format_names_the_label_the_device_last_and_a_root_only_when_it_has_one() {
        let plain = format_request("/dev/nbd0", None, false);
        assert_eq!(
            plain.command,
            ["mke2fs", "-q", "-t", "ext4", "-L", FILESYSTEM_LABEL, "/dev/nbd0"]
        );
        assert_eq!(plain.timeout, CommandRequest::new(&[]).timeout);

        let directory = tempfile::tempdir().unwrap();
        let archive = initial_contents::tests::archive();
        let staged = initial_contents::tests::staging(directory.path(), archive.clone())
            .stage(
                &VolumeId::parse("vol-1").unwrap(),
                &crate::test_support::initial_contents(&archive, "/"),
            )
            .await
            .unwrap();
        let seeded = format_request("/var/lib/nibrunner/volumes/vol-1", Some(&staged), true);
        assert_eq!(
            seeded.command,
            [
                "mke2fs",
                "-q",
                "-t",
                "ext4",
                "-F",
                "-L",
                FILESYSTEM_LABEL,
                "-d",
                &staged.path().display().to_string(),
                "/var/lib/nibrunner/volumes/vol-1"
            ]
        );
        assert_eq!(seeded.timeout, SEEDED_FORMAT_TIMEOUT);
    }

    #[test]
    fn a_size_that_would_overflow_a_sector_count_is_not_rounded_into_nothing() {
        assert_eq!(align_to_sector(u64::MAX - SECTOR_SIZE_BYTES), u64::MAX - 511);
    }
}
