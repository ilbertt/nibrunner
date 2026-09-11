use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use backhand::{compression::Compressor, FilesystemCompressor, FilesystemWriter, NodeHeader};
use protocol::{ArtifactKind, DesiredArtifact};

use crate::json_store::make_directory;
use crate::ports::{ArtifactError, ArtifactStore, ArtifactStoreExt, PayloadBuilder, PreparedPayload};

pub const ARTIFACT_IMAGE_FILENAME: &str = "artifact.squashfs";
/// A rootfs is attached as it was uploaded, so its name says nothing about its format. It sits
/// beside the packed image rather than in its place: the digest is of what was uploaded, and the
/// same bytes packed and attached whole are two different files.
pub const ROOTFS_IMAGE_FILENAME: &str = "rootfs.image";

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
const EXT4_MAGIC: [u8; 2] = [0x53, 0xEF];
const EXT4_MAGIC_OFFSET: usize = 1024 + 56;

const GUEST_BINARY_NAME: &str = "server";

pub const BINARY_MODE: u16 = 0o755;
const CONFIG_MODE: u16 = 0o600;
const CACHE_DIR_MODE: u32 = 0o755;
const VM_DIR_MODE: u32 = 0o700;

fn image_compressor() -> FilesystemCompressor {
    FilesystemCompressor::new(Compressor::Gzip, None).expect("gzip needs no options")
}

const FIXED_MTIME: u32 = 0;

fn header(permissions: u16) -> NodeHeader {
    NodeHeader {
        permissions,
        uid: 0,
        gid: 0,
        mtime: FIXED_MTIME,
    }
}

fn pack(files: &[(&str, &[u8], u16)]) -> Result<Vec<u8>, ArtifactError> {
    let mut writer = FilesystemWriter::default();
    writer.set_compressor(image_compressor());
    writer.set_time(FIXED_MTIME);
    writer.set_root_mode(0o755);
    for (name, bytes, permissions) in files {
        writer
            .push_file(
                Cursor::new(bytes.to_vec()),
                format!("/{name}"),
                header(*permissions),
            )
            .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    }
    let mut image = Cursor::new(Vec::new());
    writer
        .write(&mut image)
        .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    Ok(image.into_inner())
}

fn image_filename(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Executable => ARTIFACT_IMAGE_FILENAME,
        ArtifactKind::Rootfs => ROOTFS_IMAGE_FILENAME,
    }
}

pub fn artifact_image_path(cache_dir: &Path, artifact: &DesiredArtifact) -> PathBuf {
    cache_dir
        .join(artifact.digest.as_str())
        .join(image_filename(artifact.kind))
}

fn is_mountable_rootfs(bytes: &[u8]) -> bool {
    bytes.starts_with(SQUASHFS_MAGIC)
        || bytes
            .get(EXT4_MAGIC_OFFSET..EXT4_MAGIC_OFFSET + 2)
            .is_some_and(|magic| magic == EXT4_MAGIC)
}

/// The image a microVM gets on its artifact drive, whatever the artifact is: a binary packed as
/// the only file on a read-only image the guest's init execs, or a root filesystem attached as it
/// was uploaded for that init to stack a writable layer over.
pub struct ArtifactImages {
    store: Arc<dyn ArtifactStore>,
    cache_dir: PathBuf,
}

impl ArtifactImages {
    pub fn new(store: Arc<dyn ArtifactStore>, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { store, cache_dir })
    }
}

#[async_trait]
impl PayloadBuilder for ArtifactImages {
    async fn prepare(&self, artifact: &DesiredArtifact) -> Result<PreparedPayload, ArtifactError> {
        let artifact_image_path = ensure_artifact_image(&self.store, &self.cache_dir, artifact).await?;
        Ok(PreparedPayload { artifact_image_path })
    }
}

pub async fn ensure_artifact_image(
    store: &Arc<dyn ArtifactStore>,
    cache_dir: &Path,
    artifact: &DesiredArtifact,
) -> Result<PathBuf, ArtifactError> {
    let image_path = artifact_image_path(cache_dir, artifact);
    if image_path.exists() {
        let _ = std::fs::File::open(&image_path).and_then(|file| {
            file.set_times(
                std::fs::FileTimes::new()
                    .set_accessed(std::time::SystemTime::now())
                    .set_modified(std::time::SystemTime::now()),
            )
        });
        return Ok(image_path);
    }

    let bytes = store.read_verified(artifact).await?;

    let image = match artifact.kind {
        ArtifactKind::Executable => pack(&[(GUEST_BINARY_NAME, &bytes, BINARY_MODE)])?,
        ArtifactKind::Rootfs => {
            if !is_mountable_rootfs(&bytes) {
                return Err(ArtifactError::Unpackable(
                    "the rootfs is neither a squashfs nor an ext4 image".into(),
                ));
            }
            bytes
        }
    };
    let directory = image_path
        .parent()
        .expect("the image is one level inside the cache");
    make_directory(directory, CACHE_DIR_MODE)
        .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    let staged = directory.join(format!(
        "{}.{}.tmp",
        image_filename(artifact.kind),
        std::process::id()
    ));
    std::fs::write(&staged, &image).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    std::fs::rename(&staged, &image_path).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    tracing::info!(digest = %artifact.digest, kind = artifact.kind.as_str(), size_bytes = artifact.size_bytes, image_bytes = image.len(), "artifact image built");
    Ok(image_path)
}

pub fn build_instance_config_image(working_dir: &Path, rendered: &str) -> Result<PathBuf, ArtifactError> {
    make_directory(working_dir, VM_DIR_MODE).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    let image = pack(&[(
        guest_contract::instance_env::INSTANCE_ENV_FILENAME,
        rendered.as_bytes(),
        CONFIG_MODE,
    )])?;
    let image_path = working_dir.join(guest_contract::instance_env::INSTANCE_CONFIG_IMAGE);
    let staged = working_dir.join(format!("config.{}.tmp", std::process::id()));
    std::fs::write(&staged, &image).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&staged, &image_path).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    Ok(image_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mocks;
    use crate::test_support::{artifact, ARTIFACT_BYTES, ARTIFACT_DIGEST};

    fn artifact_bytes() -> Vec<u8> {
        ARTIFACT_BYTES.to_vec()
    }

    fn store(bytes: Vec<u8>) -> Arc<dyn ArtifactStore> {
        mocks::artifacts_holding(bytes)
    }

    fn read_back(image: &[u8], path: &str) -> Vec<u8> {
        use std::io::Read;
        let filesystem = backhand::FilesystemReader::from_reader(Cursor::new(image.to_vec())).unwrap();
        let node = filesystem
            .files()
            .find(|node| node.fullpath.to_string_lossy() == path)
            .unwrap_or_else(|| panic!("the image holds nothing at {path}"));
        let backhand::InnerNode::File(file) = &node.inner else {
            panic!("{path} is not a file");
        };
        let mut bytes = Vec::new();
        filesystem.file(file).reader().read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[tokio::test]
    async fn an_image_is_built_once_per_digest_and_holds_the_binary_at_the_boot_path() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(artifact_bytes());
        let image_path = ensure_artifact_image(&store, directory.path(), &artifact(|_| {}))
            .await
            .unwrap();
        assert!(image_path.starts_with(directory.path().join(ARTIFACT_DIGEST)));
        let image = std::fs::read(&image_path).unwrap();
        assert_eq!(&image[..4], b"hsqs");
        assert_eq!(read_back(&image, "/server"), artifact_bytes());

        let before = std::fs::metadata(&image_path).unwrap().len();
        let again = ensure_artifact_image(&store, directory.path(), &artifact(|_| {}))
            .await
            .unwrap();
        assert_eq!(again, image_path);
        assert_eq!(std::fs::metadata(&image_path).unwrap().len(), before);
        let siblings = std::fs::read_dir(image_path.parent().unwrap()).unwrap().count();
        assert_eq!(siblings, 1);
    }

    #[tokio::test]
    async fn bytes_that_are_not_what_they_claim_never_reach_a_guest() {
        let directory = tempfile::tempdir().unwrap();
        let wrong = store(b"something else entirely\n".to_vec());
        let error = ensure_artifact_image(&wrong, directory.path(), &artifact(|_| {}))
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::DigestMismatch { .. }));
        assert!(error.message().contains("not to the"));
        assert!(!artifact_image_path(directory.path(), &artifact(|_| {})).exists());

        let store = store(artifact_bytes());
        let mismatched = artifact(|artifact| artifact.size_bytes = 1);
        let error = ensure_artifact_image(&store, directory.path(), &mismatched)
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::SizeMismatch { .. }));
    }

    #[test]
    fn the_config_image_is_rebuilt_in_place_on_every_boot() {
        let directory = tempfile::tempdir().unwrap();
        let first = build_instance_config_image(directory.path(), "NIBRUN_HTTP_PORT=3000\n").unwrap();
        assert!(first.ends_with(guest_contract::instance_env::INSTANCE_CONFIG_IMAGE));
        let image = std::fs::read(&first).unwrap();
        assert_eq!(&image[..4], b"hsqs");
        assert_eq!(read_back(&image, "/instance.env"), b"NIBRUN_HTTP_PORT=3000\n");

        let second = build_instance_config_image(directory.path(), "NIBRUN_HTTP_PORT=8080\n").unwrap();
        assert_eq!(second, first);
        let rebuilt = std::fs::read(&second).unwrap();
        assert_eq!(read_back(&rebuilt, "/instance.env"), b"NIBRUN_HTTP_PORT=8080\n");
    }

    #[test]
    fn packing_the_same_bytes_twice_is_byte_identical() {
        let once = pack(&[("server", &artifact_bytes(), BINARY_MODE)]).unwrap();
        let twice = pack(&[("server", &artifact_bytes(), BINARY_MODE)]).unwrap();
        assert_eq!(once, twice);
    }

    #[tokio::test]
    async fn a_store_that_will_not_hand_over_the_bytes_is_not_read_as_an_empty_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let refusing = mocks::artifacts_refusing(ArtifactError::Transfer("no such key".into()))
            as Arc<dyn ArtifactStore>;
        let error = ensure_artifact_image(&refusing, directory.path(), &artifact(|_| {}))
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::Transfer(_)), "{error}");
        assert!(!artifact_image_path(directory.path(), &artifact(|_| {})).exists());
    }

    #[tokio::test]
    async fn bytes_that_match_are_handed_back_whole_before_anything_is_built_from_them() {
        let store = store(artifact_bytes());
        assert_eq!(
            store.read_verified(&artifact(|_| {})).await.unwrap(),
            artifact_bytes()
        );
    }

    #[tokio::test]
    async fn the_executable_payload_is_the_cached_image_and_nothing_more() {
        let directory = tempfile::tempdir().unwrap();
        let payloads = ArtifactImages::new(store(artifact_bytes()), directory.path().to_path_buf());
        let prepared = payloads.prepare(&artifact(|_| {})).await.unwrap();
        assert_eq!(
            prepared.artifact_image_path,
            artifact_image_path(directory.path(), &artifact(|_| {}))
        );
        assert!(prepared.artifact_image_path.exists());
    }

    #[test]
    fn every_digest_gets_its_own_place_in_the_cache_so_a_redeploy_never_reads_the_old_binary() {
        let cache = Path::new("/var/lib/nibrunner/artifacts");
        let one = artifact_image_path(cache, &artifact(|_| {}));
        let other = artifact_image_path(
            cache,
            &artifact(|artifact| {
                artifact.digest = protocol::Sha256Digest::parse(
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
                .unwrap();
            }),
        );
        assert_ne!(one, other);
        assert!(one.starts_with(cache));
        assert!(one.ends_with(ARTIFACT_IMAGE_FILENAME));
    }

    #[test]
    fn the_same_bytes_as_a_binary_and_as_a_rootfs_are_two_files_under_one_digest() {
        let cache = Path::new("/var/lib/nibrunner/artifacts");
        let packed = artifact_image_path(cache, &artifact(|_| {}));
        let whole = artifact_image_path(cache, &rootfs(|_| {}));
        assert_eq!(packed.parent(), whole.parent());
        assert_ne!(packed, whole);
        assert!(whole.ends_with(ROOTFS_IMAGE_FILENAME));
    }

    fn rootfs(edit: impl FnOnce(&mut DesiredArtifact)) -> DesiredArtifact {
        artifact(|artifact| {
            artifact.kind = ArtifactKind::Rootfs;
            edit(artifact);
        })
    }

    fn squashfs_bytes() -> Vec<u8> {
        pack(&[("init", b"#!/bin/sh\n", BINARY_MODE)]).unwrap()
    }

    fn ext4_bytes() -> Vec<u8> {
        let mut bytes = vec![0u8; 4096];
        bytes[EXT4_MAGIC_OFFSET..EXT4_MAGIC_OFFSET + 2].copy_from_slice(&EXT4_MAGIC);
        bytes
    }

    fn describing(bytes: &[u8]) -> DesiredArtifact {
        use sha2::Digest;
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        rootfs(|artifact| {
            artifact.digest = protocol::Sha256Digest::parse(&digest).unwrap();
            artifact.size_bytes = bytes.len() as u64;
        })
    }

    #[tokio::test]
    async fn a_rootfs_is_cached_as_it_was_uploaded_rather_than_packed() {
        for image in [squashfs_bytes(), ext4_bytes()] {
            let directory = tempfile::tempdir().unwrap();
            let wanted = describing(&image);
            let cached = ensure_artifact_image(&store(image.clone()), directory.path(), &wanted)
                .await
                .unwrap();
            assert!(cached.ends_with(ROOTFS_IMAGE_FILENAME));
            assert_eq!(std::fs::read(&cached).unwrap(), image);
        }
    }

    #[tokio::test]
    async fn bytes_that_no_kernel_could_mount_are_refused_on_the_host_rather_than_in_the_guest() {
        let directory = tempfile::tempdir().unwrap();
        let junk = b"not a filesystem at all, however you look at it".to_vec();
        let wanted = describing(&junk);
        let error = ensure_artifact_image(&store(junk), directory.path(), &wanted)
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::Unpackable(_)), "{error}");
        assert!(
            error.message().contains("neither a squashfs nor an ext4"),
            "{error}"
        );
        assert!(!artifact_image_path(directory.path(), &wanted).exists());

        let directory = tempfile::tempdir().unwrap();
        let as_binary = artifact(|artifact| {
            artifact.digest = wanted.digest.clone();
            artifact.size_bytes = wanted.size_bytes;
        });
        let junk = b"not a filesystem at all, however you look at it".to_vec();
        assert!(
            ensure_artifact_image(&store(junk), directory.path(), &as_binary)
                .await
                .is_ok(),
            "the same bytes are a fine binary; only a rootfs has a shape to check"
        );
    }

    #[test]
    fn an_image_that_has_nowhere_to_be_written_is_named_rather_than_left_half_built() {
        let directory = tempfile::tempdir().unwrap();
        let occupied = directory.path().join("vm");
        std::fs::write(&occupied, b"a file, not a directory").unwrap();
        let error = build_instance_config_image(&occupied, "NIBRUN_HTTP_PORT=3000\n").unwrap_err();
        assert!(matches!(error, ArtifactError::Unpackable(_)), "{error}");
    }

    #[test]
    fn the_binary_a_guest_runs_is_packed_executable_and_the_config_it_reads_is_not() {
        let image = pack(&[
            ("server", &artifact_bytes(), BINARY_MODE),
            ("instance.env", b"NIBRUN_HTTP_PORT=3000\n", CONFIG_MODE),
        ])
        .unwrap();
        let filesystem = backhand::FilesystemReader::from_reader(Cursor::new(image)).unwrap();
        let mode = |path: &str| {
            filesystem
                .files()
                .find(|node| node.fullpath.to_string_lossy() == path)
                .map(|node| node.header.permissions)
                .unwrap_or_else(|| panic!("the image holds nothing at {path}"))
        };
        assert_eq!(mode("/server"), BINARY_MODE);
        assert_eq!(mode("/instance.env"), CONFIG_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn a_config_image_on_disk_is_readable_only_by_the_user_this_daemon_runs_as() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let image = build_instance_config_image(directory.path(), "NIBRUN_HTTP_PORT=3000\n").unwrap();
        assert_eq!(
            std::fs::metadata(&image).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let siblings = std::fs::read_dir(directory.path()).unwrap().count();
        assert_eq!(siblings, 1, "nothing staged is left beside the image");
    }

    #[tokio::test]
    async fn an_image_already_in_the_cache_is_touched_so_a_reap_takes_the_cold_ones_first() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(artifact_bytes());
        let image_path = ensure_artifact_image(&store, directory.path(), &artifact(|_| {}))
            .await
            .unwrap();
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&image_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(backdated))
            .unwrap();

        ensure_artifact_image(&store, directory.path(), &artifact(|_| {}))
            .await
            .unwrap();
        let touched = std::fs::metadata(&image_path).unwrap().modified().unwrap();
        assert!(touched > backdated, "the image was not touched on a cache hit");
    }
}
