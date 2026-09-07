use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DesiredVolume, ObjectKey, VolumeId};

use crate::adapters::volumes::{
    align_to_sector, has_ext_magic, AttachedVolume, ObservedBacking, VolumeBackend, VolumeError,
    FILESYSTEM_LABEL, SUPERBLOCK_MAGIC_OFFSET,
};
use crate::json_store::make_directory;
use crate::ports::{CommandRequest, CommandRunner, CommandRunnerExt};

const VOLUME_DIR_MODE: u32 = 0o700;
const VOLUME_FILE_MODE: u32 = 0o600;

pub struct LocalFileVolumes {
    directory: PathBuf,
    storage_prefix: ObjectKey,
    commands: Arc<dyn CommandRunner>,
}

impl LocalFileVolumes {
    pub fn new(directory: PathBuf, storage_prefix: ObjectKey, commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            directory,
            storage_prefix,
            commands,
        }
    }

    pub fn path_for(&self, volume_id: &VolumeId) -> PathBuf {
        self.directory.join(volume_id.as_str())
    }

    fn size_of(path: &Path) -> Option<u64> {
        std::fs::metadata(path)
            .ok()
            .filter(|info| info.is_file())
            .map(|info| info.len())
    }

    fn ensure_file(&self, volume_id: &VolumeId, size_bytes: u64) -> Result<u64, VolumeError> {
        let path = self.path_for(volume_id);
        let target = align_to_sector(size_bytes);
        if let Some(current) = Self::size_of(&path) {
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
        make_directory(&self.directory, VOLUME_DIR_MODE).map_err(|error| {
            VolumeError::Unusable(format!("{} could not be made: {error}", self.directory.display()))
        })?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                VolumeError::Unusable(format!("{} could not be opened: {error}", path.display()))
            })?;
        file.set_len(target).map_err(|error| {
            VolumeError::Unusable(format!("{} could not be sized: {error}", path.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(VOLUME_FILE_MODE));
        }
        Ok(target)
    }

    fn is_formatted(&self, volume_id: &VolumeId) -> Result<bool, VolumeError> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.path_for(volume_id);
        let mut file = std::fs::File::open(&path).map_err(|_| VolumeError::SuperblockUnreadable {
            device_path: path.display().to_string(),
        })?;
        if file.seek(SeekFrom::Start(SUPERBLOCK_MAGIC_OFFSET)).is_err() {
            return Err(VolumeError::SuperblockUnreadable {
                device_path: path.display().to_string(),
            });
        }
        let mut magic = [0u8; 2];
        match file.read_exact(&mut magic) {
            Ok(()) => Ok(has_ext_magic(&magic)),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(_) => Err(VolumeError::SuperblockUnreadable {
                device_path: path.display().to_string(),
            }),
        }
    }

    async fn format_once(&self, volume_id: &VolumeId) -> Result<bool, VolumeError> {
        if self.is_formatted(volume_id)? {
            return Ok(false);
        }
        let path = self.path_for(volume_id);
        self.commands
            .stdout_of(CommandRequest::new(&[
                "mke2fs",
                "-q",
                "-t",
                "ext4",
                "-F",
                "-L",
                FILESYSTEM_LABEL,
                &path.display().to_string(),
            ]))
            .await
            .map_err(|error| VolumeError::Unusable(error.message()))?;
        Ok(true)
    }

    fn attached(&self, volume_id: &VolumeId, size_bytes: u64) -> AttachedVolume {
        AttachedVolume {
            volume_id: volume_id.clone(),
            device_path: self.path_for(volume_id).display().to_string(),
            size_bytes,
            storage_prefix: self.storage_prefix.clone(),
        }
    }
}

#[async_trait]
impl VolumeBackend for LocalFileVolumes {
    async fn provision(&self, desired: &DesiredVolume) -> Result<AttachedVolume, VolumeError> {
        let size_bytes = self.ensure_file(&desired.volume_id, desired.size_bytes)?;
        if self.format_once(&desired.volume_id).await? {
            tracing::info!(volume_id = %desired.volume_id, "volume formatted");
        }
        Ok(self.attached(&desired.volume_id, size_bytes))
    }

    async fn attach(
        &self,
        volume_id: &VolumeId,
        _app_id: &protocol::AppId,
    ) -> Result<AttachedVolume, VolumeError> {
        let size_bytes = Self::size_of(&self.path_for(volume_id)).ok_or_else(|| VolumeError::NotHere {
            volume_id: volume_id.clone(),
        })?;
        Ok(self.attached(volume_id, size_bytes))
    }

    async fn detach(&self, _volume_id: &VolumeId, _app_id: &protocol::AppId) -> Result<(), VolumeError> {
        Ok(())
    }

    async fn teardown(&self, volume_id: &VolumeId, _app_id: &protocol::AppId) -> Result<(), VolumeError> {
        let path = self.path_for(volume_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(VolumeError::Unusable(format!(
                "{} could not be removed: {error}",
                path.display()
            ))),
        }
    }

    async fn flush(&self) -> Result<(), VolumeError> {
        Ok(())
    }

    async fn create_checkpoint(&self, _checkpoint_id: &protocol::CheckpointId) -> Result<(), VolumeError> {
        Err(VolumeError::NoCheckpoints {
            what: "a volume kept as a file on this host's own disk",
        })
    }

    async fn delete_checkpoint(&self, _checkpoint_id: &protocol::CheckpointId) -> Result<(), VolumeError> {
        Err(VolumeError::NoCheckpoints {
            what: "a volume kept as a file on this host's own disk",
        })
    }

    async fn observe_checkpoints(&self) -> Vec<protocol::CheckpointId> {
        Vec::new()
    }

    async fn observe(
        &self,
        _owners: &std::collections::BTreeMap<VolumeId, protocol::AppId>,
    ) -> Vec<ObservedBacking> {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let volume_id = VolumeId::parse(entry.file_name().to_string_lossy().as_ref()).ok()?;
                let size_bytes = Self::size_of(&entry.path())?;
                Some(ObservedBacking {
                    device_path: Some(entry.path().display().to_string()),
                    attached: true,
                    size_bytes,
                    storage_prefix: self.storage_prefix.clone(),
                    volume_id,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{CommandResult, MockCommandRunner};
    use crate::test_support::mocks;
    use crate::test_support::{app_id, checkpoint_id, desired_volume, volume_id, VOLUME_SIZE_BYTES};

    fn backend(directory: &Path, commands: Arc<MockCommandRunner>) -> LocalFileVolumes {
        LocalFileVolumes::new(
            directory.to_path_buf(),
            ObjectKey::parse("volumes").unwrap(),
            commands,
        )
    }

    #[tokio::test]
    async fn a_volume_is_formatted_once_and_never_again() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, log) = mocks::commands_answering(|request| {
            let path = request.command.last().expect("a path to format");
            let mut image = std::fs::read(path).unwrap_or_default();
            image.resize(4096, 0);
            image[SUPERBLOCK_MAGIC_OFFSET as usize..SUPERBLOCK_MAGIC_OFFSET as usize + 2]
                .copy_from_slice(&0xef53u16.to_le_bytes());
            std::fs::write(path, image).unwrap();
            Ok(CommandResult::succeeded())
        });
        let volumes = backend(directory.path(), commands);
        let attached = volumes.provision(&desired_volume(|_| {})).await.unwrap();
        assert_eq!(attached.size_bytes, VOLUME_SIZE_BYTES);
        assert_eq!(log.executables(), vec!["mke2fs"]);
        assert!(log.calls()[0].command.contains(&"-L".to_string()));
        assert!(log.calls()[0].command.contains(&FILESYSTEM_LABEL.to_string()));

        volumes.provision(&desired_volume(|_| {})).await.unwrap();
        assert_eq!(
            log.executables().len(),
            1,
            "a formatted volume is not formatted again"
        );
    }

    #[tokio::test]
    async fn a_volume_grows_and_is_never_shrunk() {
        let directory = tempfile::tempdir().unwrap();
        let volumes = backend(directory.path(), mocks::commands_succeeding().0);
        volumes.provision(&desired_volume(|_| {})).await.unwrap();
        let grown = volumes
            .provision(&desired_volume(|volume| {
                volume.size_bytes = VOLUME_SIZE_BYTES * 2
            }))
            .await
            .unwrap();
        assert_eq!(grown.size_bytes, VOLUME_SIZE_BYTES * 2);
        let refused = volumes.provision(&desired_volume(|_| {})).await.unwrap_err();
        assert_eq!(
            refused,
            VolumeError::ShrinkRefused {
                current: VOLUME_SIZE_BYTES * 2,
                requested: VOLUME_SIZE_BYTES
            }
        );
    }

    #[tokio::test]
    async fn a_size_that_is_not_a_whole_sector_is_rounded_up() {
        let directory = tempfile::tempdir().unwrap();
        let volumes = backend(directory.path(), mocks::commands_succeeding().0);
        let attached = volumes
            .provision(&desired_volume(|volume| volume.size_bytes = 1000))
            .await
            .unwrap();
        assert_eq!(attached.size_bytes, 1024);
    }

    #[tokio::test]
    async fn what_the_host_holds_is_observed_from_the_disk_rather_than_remembered() {
        let directory = tempfile::tempdir().unwrap();
        let volumes = backend(directory.path(), mocks::commands_succeeding().0);
        assert!(volumes.observe(&Default::default()).await.is_empty());
        volumes.provision(&desired_volume(|_| {})).await.unwrap();
        std::fs::write(directory.path().join("not a volume"), b"").unwrap();
        let observed = volumes.observe(&Default::default()).await;
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].volume_id, volume_id());
        assert!(observed[0].attached);
        assert_eq!(observed[0].size_bytes, VOLUME_SIZE_BYTES);

        volumes.teardown(&volume_id(), &app_id()).await.unwrap();
        assert!(volumes.observe(&Default::default()).await.is_empty());
        volumes.teardown(&volume_id(), &app_id()).await.unwrap();
        assert!(matches!(
            volumes.attach(&volume_id(), &app_id()).await,
            Err(VolumeError::NotHere { .. })
        ));
    }

    #[tokio::test]
    async fn a_volume_kept_as_a_local_file_says_it_cannot_be_checkpointed() {
        let directory = tempfile::tempdir().unwrap();
        let volumes = backend(directory.path(), mocks::commands_succeeding().0);
        assert!(volumes.create_checkpoint(&checkpoint_id()).await.is_err());
        assert!(volumes.observe_checkpoints().await.is_empty());
        volumes.flush().await.unwrap();
    }
}
