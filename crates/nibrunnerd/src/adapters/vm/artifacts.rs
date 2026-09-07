use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use backhand::{compression::Compressor, FilesystemCompressor, FilesystemWriter, NodeHeader};
use protocol::{DesiredArtifact, Sha256Digest};

use crate::json_store::make_directory;
use crate::ports::{ArtifactError, ArtifactStore};

pub const ARTIFACT_IMAGE_FILENAME: &str = "artifact.squashfs";

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

fn digest_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

pub fn artifact_image_path(cache_dir: &Path, digest: &Sha256Digest) -> PathBuf {
    cache_dir.join(digest.as_str()).join(ARTIFACT_IMAGE_FILENAME)
}

pub async fn fetch_verified(
    store: &Arc<dyn ArtifactStore>,
    artifact: &DesiredArtifact,
) -> Result<Vec<u8>, ArtifactError> {
    let bytes = store.read(&artifact.object_key).await?;
    let actual = digest_of(&bytes);
    if actual != artifact.digest.as_str() {
        return Err(ArtifactError::DigestMismatch {
            expected: artifact.digest.clone(),
            actual,
        });
    }
    if bytes.len() as u64 != artifact.size_bytes {
        return Err(ArtifactError::SizeMismatch {
            expected: artifact.size_bytes,
            actual: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

pub async fn ensure_artifact_image(
    store: &Arc<dyn ArtifactStore>,
    cache_dir: &Path,
    artifact: &DesiredArtifact,
) -> Result<PathBuf, ArtifactError> {
    let image_path = artifact_image_path(cache_dir, &artifact.digest);
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

    let bytes = fetch_verified(store, artifact).await?;

    let image = pack(&[(GUEST_BINARY_NAME, &bytes, BINARY_MODE)])?;
    let directory = image_path
        .parent()
        .expect("the image is one level inside the cache");
    make_directory(directory, CACHE_DIR_MODE)
        .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    let staged = directory.join(format!("{ARTIFACT_IMAGE_FILENAME}.{}.tmp", std::process::id()));
    std::fs::write(&staged, &image).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    std::fs::rename(&staged, &image_path).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    tracing::info!(digest = %artifact.digest, size_bytes = artifact.size_bytes, image_bytes = image.len(), "artifact image built");
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

    #[test]
    fn the_fixture_digest_is_the_digest_of_the_fixture_bytes() {
        assert_eq!(digest_of(&artifact_bytes()), ARTIFACT_DIGEST);
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
        assert!(!artifact_image_path(directory.path(), &artifact(|_| {}).digest).exists());

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
}
