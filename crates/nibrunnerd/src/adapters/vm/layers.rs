use std::io::{BufWriter, Cursor, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use backhand::compression::{CompressionOptions, Compressor, Zstd};
use backhand::{FilesystemCompressor, FilesystemWriter, NodeHeader};
use protocol::{DesiredLayer, OciSource};

use crate::domain::metrics::layers::{LayerMetrics, LayerPhase};
use crate::domain::metrics::HostMetrics;
use crate::json_store::make_directory;
use crate::ports::{
    verify_artifact, ArtifactError, ArtifactStore, CommandRunner, PayloadBuilder, PreparedPayload,
};

mod oci;
mod registry;

const MAX_PREPARATIONS: usize = 2;
static PREPARATION: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_PREPARATIONS)));

pub const VERBATIM_IMAGE_FILENAME: &str = "layer.img";

pub const BINARY_MODE: u16 = 0o755;
const CONFIG_MODE: u16 = 0o600;
const DIRECTORY_MODE: u16 = 0o755;
const CACHE_DIR_MODE: u32 = 0o755;
const VM_DIR_MODE: u32 = 0o700;

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
const EXT4_MAGIC_OFFSET: usize = 0x438;
const EXT4_MAGIC: [u8; 2] = [0x53, 0xEF];

// Layer compression is on the startup path, so favor packing speed over smaller cached images.
const COMPRESSION_LEVEL: i32 = -3;

fn image_compressor() -> FilesystemCompressor {
    FilesystemCompressor::new(
        Compressor::Zstd,
        Some(CompressionOptions::Zstd(Zstd {
            // Squashfs stores a signed level; backhand represents its bits as u32.
            compression_level: COMPRESSION_LEVEL as u32,
        })),
    )
    .expect("zstd takes a level and nothing else")
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

/// Builds a squashfs holding exactly these files at these absolute paths, byte-identical for the
/// same input so an image is cached by what went into it.
fn pack(files: &[(&str, &[u8], u16)]) -> Result<Vec<u8>, ArtifactError> {
    let mut image = Cursor::new(Vec::new());
    pack_into(files, &mut image)?;
    Ok(image.into_inner())
}

fn pack_into(files: &[(&str, &[u8], u16)], image: &mut (impl Write + Seek)) -> Result<u64, ArtifactError> {
    let unpackable = |error: backhand::BackhandError| ArtifactError::Unpackable(error.to_string());
    let mut writer = FilesystemWriter::default();
    writer.set_compressor(image_compressor());
    writer.set_time(FIXED_MTIME);
    writer.set_root_mode(DIRECTORY_MODE);
    for (path, bytes, permissions) in files {
        if let Some(parent) = Path::new(path)
            .parent()
            .filter(|parent| *parent != Path::new("/"))
        {
            writer
                .push_dir_all(parent, header(DIRECTORY_MODE))
                .map_err(unpackable)?;
        }
        writer
            .push_file(Cursor::new(*bytes), path, header(*permissions))
            .map_err(unpackable)?;
    }
    writer.write(image).map(|(_, bytes)| bytes).map_err(unpackable)
}

pub fn is_filesystem_image(bytes: &[u8]) -> bool {
    bytes.starts_with(SQUASHFS_MAGIC)
        || bytes
            .get(EXT4_MAGIC_OFFSET..EXT4_MAGIC_OFFSET + EXT4_MAGIC.len())
            .is_some_and(|magic| magic == EXT4_MAGIC)
}

fn short_hash(text: &str) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(text.as_bytes())[..8])
}

/// Where a layer's image lives in the cache. The object is the same bytes whether it is attached
/// whole or packed as a program at some path, so the kind and the path are part of the name.
pub fn layer_image_path(cache_dir: &Path, layer: &DesiredLayer) -> PathBuf {
    let directory = cache_dir.join(layer.digest().as_str());
    match layer {
        DesiredLayer::Oci { .. } => directory.join("oci.ext4"),
        DesiredLayer::Filesystem { .. } => directory.join(VERBATIM_IMAGE_FILENAME),
        DesiredLayer::Executable { destination_path, .. } => directory.join(format!(
            "executable-{}.squashfs",
            short_hash(destination_path.as_str())
        )),
    }
}

pub struct LayerImages {
    store: Arc<dyn ArtifactStore>,
    cache_dir: PathBuf,
    commands: Arc<dyn CommandRunner>,
    metrics: Arc<HostMetrics>,
}

impl LayerImages {
    pub fn new(
        store: Arc<dyn ArtifactStore>,
        cache_dir: PathBuf,
        commands: Arc<dyn CommandRunner>,
        metrics: Arc<HostMetrics>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            cache_dir,
            commands,
            metrics,
        })
    }
}

#[async_trait]
impl PayloadBuilder for LayerImages {
    async fn prepare(&self, layers: &[DesiredLayer]) -> Result<PreparedPayload, ArtifactError> {
        let mut layer_image_paths = Vec::with_capacity(layers.len());
        let mut fetched_bytes = 0;
        for layer in layers {
            let image =
                ensure_layer_image(&self.store, &self.cache_dir, layer, &self.commands, &self.metrics)
                    .await?;
            layer_image_paths.push(image.path);
            fetched_bytes += image.fetched_bytes;
        }
        Ok(PreparedPayload {
            layer_image_paths,
            fetched_bytes,
        })
    }
}

/// A layer's image in the cache, and what putting it there cost: nothing for one already held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerImage {
    pub path: PathBuf,
    pub fetched_bytes: u64,
}

pub async fn ensure_layer_image(
    store: &Arc<dyn ArtifactStore>,
    cache_dir: &Path,
    layer: &DesiredLayer,
    commands: &Arc<dyn CommandRunner>,
    metrics: &HostMetrics,
) -> Result<LayerImage, ArtifactError> {
    let image_path = layer_image_path(cache_dir, layer);
    if let Some(cached) = cached_image(&image_path) {
        return Ok(cached);
    }
    // A miss retains an artifact while packing it, so reserve capacity before reading it.
    let queued = std::time::Instant::now();
    let permit = PREPARATION
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    metrics.layers.observed(LayerPhase::Queue, true, queued.elapsed());
    let permit = Arc::new(permit);
    if let Some(cached) = cached_image(&image_path) {
        return Ok(cached);
    }

    let (bytes, fetched_bytes) = match layer {
        DesiredLayer::Oci {
            source: OciSource::Registry { repository, digest },
        } => {
            let pulled = registry::pull(repository, digest).await?;
            (pulled.archive, pulled.fetched_bytes)
        }
        DesiredLayer::Oci {
            source: OciSource::Archive(object),
        }
        | DesiredLayer::Filesystem { object }
        | DesiredLayer::Executable { object, .. } => {
            let reading = std::time::Instant::now();
            let result = store.read(&object.object_key).await;
            metrics
                .layers
                .observed(LayerPhase::Read, result.is_ok(), reading.elapsed());
            let bytes = result?;
            let object = object.clone();
            let measured = metrics.layers.clone();
            let bytes = in_worker(&permit, move || {
                let verifying = std::time::Instant::now();
                let result = verify_artifact(&bytes, &object);
                measured.observed(LayerPhase::Verify, result.is_ok(), verifying.elapsed());
                result?;
                Ok(bytes)
            })
            .await?;
            let fetched_bytes = bytes.len() as u64;
            (bytes, fetched_bytes)
        }
    };
    if matches!(layer, DesiredLayer::Oci { .. }) {
        oci::prepare(bytes, image_path.clone(), commands).await?;
        return Ok(LayerImage {
            path: image_path,
            fetched_bytes,
        });
    }
    let layer = layer.clone();
    let measured = metrics.layers.clone();
    in_worker(&permit, move || {
        write_stored_image(&layer, bytes, image_path, fetched_bytes, &measured)
    })
    .await
}

fn cached_image(image_path: &Path) -> Option<LayerImage> {
    if !image_path.exists() {
        return None;
    }
    let _ = std::fs::File::open(image_path).and_then(|file| {
        file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(std::time::SystemTime::now())
                .set_modified(std::time::SystemTime::now()),
        )
    });
    Some(LayerImage {
        path: image_path.to_path_buf(),
        fetched_bytes: 0,
    })
}

async fn in_worker<T: Send + 'static>(
    permit: &Arc<tokio::sync::OwnedSemaphorePermit>,
    work: impl FnOnce() -> Result<T, ArtifactError> + Send + 'static,
) -> Result<T, ArtifactError> {
    let permit = permit.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|error| ArtifactError::Unpackable(format!("layer preparation worker failed: {error}")))?
}

fn write_stored_image(
    layer: &DesiredLayer,
    bytes: Vec<u8>,
    image_path: PathBuf,
    fetched_bytes: u64,
    metrics: &LayerMetrics,
) -> Result<LayerImage, ArtifactError> {
    let preparing = std::time::Instant::now();
    let result = match layer {
        _ if !matches!(layer, DesiredLayer::Executable { .. }) && !is_filesystem_image(&bytes) => {
            return Err(ArtifactError::NotAnImage {
                digest: layer.digest().clone(),
            });
        }
        DesiredLayer::Executable { destination_path, .. } => {
            stage_executable(&image_path, destination_path.as_str(), &bytes)
        }
        _ => stage_filesystem(&image_path, &bytes),
    };
    let prepared_for = preparing.elapsed();
    let executable = matches!(layer, DesiredLayer::Executable { .. });
    if executable {
        metrics.observed(LayerPhase::Pack, result.is_ok(), prepared_for);
    } else if result.is_err() {
        metrics.observed(LayerPhase::Publish, false, prepared_for);
    }
    let PreparedImage { staged, image_bytes } = result?;
    let publishing = std::time::Instant::now();
    let result = staged.persist(&image_path);
    let published_for = publishing.elapsed()
        + if executable {
            std::time::Duration::ZERO
        } else {
            prepared_for
        };
    metrics.observed(LayerPhase::Publish, result.is_ok(), published_for);
    result.map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    tracing::info!(
        digest = %layer.digest(),
        size_bytes = fetched_bytes,
        image_bytes,
        packed = matches!(layer, DesiredLayer::Executable { .. }),
        "layer image ready"
    );
    Ok(LayerImage {
        path: image_path,
        fetched_bytes,
    })
}

struct PreparedImage {
    staged: tempfile::NamedTempFile,
    image_bytes: u64,
}

fn stage_executable(
    image_path: &Path,
    destination: &str,
    bytes: &[u8],
) -> Result<PreparedImage, ArtifactError> {
    let mut staged = stage_image(image_path)?;
    let image_bytes = {
        let mut output = BufWriter::new(staged.as_file_mut());
        let written = pack_into(&[(destination, bytes, BINARY_MODE)], &mut output)?;
        output
            .flush()
            .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
        written
    };
    Ok(PreparedImage { staged, image_bytes })
}

fn stage_filesystem(image_path: &Path, bytes: &[u8]) -> Result<PreparedImage, ArtifactError> {
    let mut staged = stage_image(image_path)?;
    staged
        .write_all(bytes)
        .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    Ok(PreparedImage {
        staged,
        image_bytes: bytes.len() as u64,
    })
}

fn stage_image(image_path: &Path) -> Result<tempfile::NamedTempFile, ArtifactError> {
    let directory = image_path
        .parent()
        .expect("the image is one level inside the cache");
    make_directory(directory, CACHE_DIR_MODE)
        .map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    tempfile::Builder::new()
        .prefix(".layer-")
        .permissions(std::fs::Permissions::from_mode(0o644))
        .tempfile_in(directory)
        .map_err(|error| ArtifactError::Unpackable(error.to_string()))
}

pub fn build_instance_config_image(working_dir: &Path, rendered: &str) -> Result<PathBuf, ArtifactError> {
    make_directory(working_dir, VM_DIR_MODE).map_err(|error| ArtifactError::Unpackable(error.to_string()))?;
    let image = pack(&[(
        &format!("/{}", guest_contract::instance_env::INSTANCE_ENV_FILENAME),
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
    use crate::ports::ArtifactStoreExt;
    use crate::test_support::mocks;
    use crate::test_support::{base_layer, layer, ARTIFACT_BYTES, ARTIFACT_DIGEST, BASE_LAYER_BYTES};
    use protocol::{ExecutablePath, Sha256Digest};

    fn artifact_bytes() -> Vec<u8> {
        ARTIFACT_BYTES.to_vec()
    }

    fn as_filesystem(layer: DesiredLayer) -> DesiredLayer {
        DesiredLayer::Filesystem {
            object: layer.stored_object().unwrap().clone(),
        }
    }

    fn placed_at(layer: DesiredLayer, path: &str) -> DesiredLayer {
        DesiredLayer::Executable {
            object: layer.stored_object().unwrap().clone(),
            destination_path: ExecutablePath::parse(path).unwrap(),
        }
    }

    fn store(bytes: Vec<u8>) -> Arc<dyn ArtifactStore> {
        mocks::artifacts_holding(bytes)
    }

    async fn prepare_layer(
        store: &Arc<dyn ArtifactStore>,
        cache_dir: &Path,
        layer: &DesiredLayer,
    ) -> Result<LayerImage, ArtifactError> {
        let commands: Arc<dyn CommandRunner> = mocks::commands_succeeding().0;
        super::ensure_layer_image(store, cache_dir, layer, &commands, &HostMetrics::new()).await
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
    async fn an_executable_is_packed_once_per_digest_and_holds_the_program_where_the_guest_runs_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(artifact_bytes());
        let fetched = prepare_layer(&store, directory.path(), &layer(|_| {}))
            .await
            .unwrap();
        let image_path = fetched.path.clone();
        assert!(image_path.starts_with(directory.path().join(ARTIFACT_DIGEST)));
        assert_eq!(
            fetched.fetched_bytes,
            ARTIFACT_BYTES.len() as u64,
            "what was pulled is what the store held, not what the image packed it into"
        );
        let image = std::fs::read(&image_path).unwrap();
        assert_eq!(&image[..4], b"hsqs");
        assert_eq!(read_back(&image, "/app/server"), artifact_bytes());

        let before = std::fs::metadata(&image_path).unwrap().len();
        let again = prepare_layer(&store, directory.path(), &layer(|_| {}))
            .await
            .unwrap();
        assert_eq!(again.path, image_path);
        assert_eq!(again.fetched_bytes, 0, "a cache hit pulls nothing");
        assert_eq!(std::fs::metadata(&image_path).unwrap().len(), before);
        let siblings = std::fs::read_dir(image_path.parent().unwrap()).unwrap().count();
        assert_eq!(siblings, 1);
    }

    fn phase_page(metrics: &HostMetrics) -> String {
        crate::domain::metrics::tests::page(
            &crate::domain::metrics::tests::report(),
            metrics,
            &crate::state::HostSnapshot::default(),
            0,
        )
    }

    #[tokio::test]
    async fn cancelling_preparation_keeps_capacity_reserved_until_its_worker_finishes() {
        let capacity = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::new(capacity.clone().acquire_owned().await.unwrap());
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let preparing = tokio::spawn(async move {
            in_worker(&permit, move || {
                started.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
            .await
        });
        running.await.unwrap();
        preparing.abort();
        assert!(preparing.await.unwrap_err().is_cancelled());
        assert!(capacity.try_acquire().is_err());

        release.send(()).unwrap();
        let available = tokio::time::timeout(std::time::Duration::from_secs(2), capacity.acquire()).await;
        assert!(available.unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_cache_hit_does_not_repeat_layer_preparation_measurements() {
        let directory = tempfile::tempdir().unwrap();
        let metrics = Arc::new(HostMetrics::new());
        let images = LayerImages::new(
            store(artifact_bytes()),
            directory.path().to_path_buf(),
            mocks::commands_succeeding().0,
            metrics.clone(),
        );
        images.prepare(&[layer(|_| {})]).await.unwrap();
        images.prepare(&[layer(|_| {})]).await.unwrap();

        let page = phase_page(&metrics);
        for phase in ["read", "verify", "pack", "publish"] {
            assert!(page.contains(&format!(
                "nibrunner_layer_preparation_phase_seconds_count{{phase=\"{phase}\",outcome=\"ok\"}} 1\n"
            )));
        }
    }

    #[tokio::test]
    async fn a_digest_mismatch_is_measured_without_packing_or_publishing() {
        let directory = tempfile::tempdir().unwrap();
        let metrics = Arc::new(HostMetrics::new());
        let images = LayerImages::new(
            store(b"wrong program".to_vec()),
            directory.path().to_path_buf(),
            mocks::commands_succeeding().0,
            metrics.clone(),
        );
        assert!(matches!(
            images.prepare(&[layer(|_| {})]).await,
            Err(ArtifactError::DigestMismatch { .. })
        ));

        let page = phase_page(&metrics);
        assert!(page.contains(
            "nibrunner_layer_preparation_phase_seconds_count{phase=\"verify\",outcome=\"failed\"} 1\n"
        ));
        for phase in ["pack", "publish"] {
            assert!(page.contains(&format!(
                "nibrunner_layer_preparation_phase_seconds_count{{phase=\"{phase}\",outcome=\"ok\"}} 0\n"
            )));
        }
    }

    #[tokio::test]
    async fn the_program_sits_where_the_document_put_it_and_the_directories_above_are_made() {
        let directory = tempfile::tempdir().unwrap();
        let deep = placed_at(layer(|_| {}), "/usr/local/bin/server");
        let image_path = prepare_layer(&store(artifact_bytes()), directory.path(), &deep)
            .await
            .unwrap()
            .path;
        let image = std::fs::read(&image_path).unwrap();
        assert_eq!(read_back(&image, "/usr/local/bin/server"), artifact_bytes());
        assert_ne!(image_path, layer_image_path(directory.path(), &layer(|_| {})));
    }

    #[tokio::test]
    async fn a_filesystem_is_attached_as_it_was_uploaded() {
        let directory = tempfile::tempdir().unwrap();
        let image_path = prepare_layer(&store(BASE_LAYER_BYTES.to_vec()), directory.path(), &base_layer())
            .await
            .unwrap()
            .path;
        assert!(image_path.ends_with(VERBATIM_IMAGE_FILENAME));
        assert_eq!(std::fs::read(&image_path).unwrap(), BASE_LAYER_BYTES);
    }

    #[tokio::test]
    async fn a_filesystem_that_is_not_one_is_refused_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let not_an_image = as_filesystem(layer(|_| {}));
        let error = prepare_layer(&store(artifact_bytes()), directory.path(), &not_an_image)
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::NotAnImage { .. }), "{error}");
        assert!(error.message().contains(ARTIFACT_DIGEST));
        assert!(!layer_image_path(directory.path(), &not_an_image).exists());
    }

    #[test]
    fn a_filesystem_is_known_by_its_magic() {
        assert!(is_filesystem_image(b"hsqs and then anything"));
        let mut ext4 = vec![0u8; 4096];
        ext4[EXT4_MAGIC_OFFSET..EXT4_MAGIC_OFFSET + 2].copy_from_slice(&EXT4_MAGIC);
        assert!(is_filesystem_image(&ext4));
        assert!(!is_filesystem_image(b"\x7fELF"));
        assert!(!is_filesystem_image(b""));
        assert!(!is_filesystem_image(&vec![0u8; 4096]));
    }

    #[tokio::test]
    async fn bytes_that_are_not_what_they_claim_never_reach_a_guest() {
        let directory = tempfile::tempdir().unwrap();
        let wrong = store(b"something else entirely\n".to_vec());
        let error = prepare_layer(&wrong, directory.path(), &layer(|_| {}))
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::DigestMismatch { .. }));
        assert!(error.message().contains("not to the"));
        assert!(!layer_image_path(directory.path(), &layer(|_| {})).exists());
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

    // The guest kernel reads the image, so what it is compressed with is a contract with the kernel
    // config the guest image is built from — and one the superblock states, at the offset where
    // squashfs keeps its compressor id.
    #[test]
    fn an_image_is_compressed_with_zstd_which_the_guest_kernel_has() {
        const COMPRESSOR_ID_OFFSET: usize = 20;
        const ZSTD: u16 = 6;
        let image = pack(&[("/app/server", &artifact_bytes(), BINARY_MODE)]).unwrap();
        let id = u16::from_le_bytes([image[COMPRESSOR_ID_OFFSET], image[COMPRESSOR_ID_OFFSET + 1]]);
        assert_eq!(id, ZSTD);
    }

    #[test]
    fn the_fast_zstd_level_is_preserved_in_the_standard_compression_options() {
        let image = pack(&[("/app/server", &artifact_bytes(), BINARY_MODE)]).unwrap();
        let filesystem = backhand::FilesystemReader::from_reader(Cursor::new(image)).unwrap();
        let Some(CompressionOptions::Zstd(options)) = filesystem.compression_options else {
            panic!("the filesystem must describe its zstd compression level");
        };
        assert_eq!(options.compression_level as i32, COMPRESSION_LEVEL);
    }

    #[test]
    fn packing_the_same_bytes_twice_is_byte_identical() {
        let once = pack(&[("/app/server", &artifact_bytes(), BINARY_MODE)]).unwrap();
        let twice = pack(&[("/app/server", &artifact_bytes(), BINARY_MODE)]).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_program_spanning_compressed_blocks_and_a_fragment_is_preserved() {
        let program: Vec<u8> = (0..backhand::DEFAULT_BLOCK_SIZE as usize * 3 + 137)
            .map(|index| (index.wrapping_mul(31) ^ (index >> 8)) as u8)
            .collect();
        let files = [("/app/server", program.as_slice(), BINARY_MODE)];
        let image = pack(&files).unwrap();

        assert_eq!(read_back(&image, "/app/server"), program);
        assert_eq!(pack(&files).unwrap(), image);
    }

    #[test]
    fn a_streamed_image_matches_the_buffered_image_and_is_readable_by_the_jailer() {
        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("program.squashfs");
        let program = vec![0x5a; backhand::DEFAULT_BLOCK_SIZE as usize * 3 + 137];
        let image = stage_executable(&image_path, "/app/server", &program).unwrap();

        assert!(!image_path.exists());
        assert_eq!(
            std::fs::read(image.staged.path()).unwrap(),
            pack(&[("/app/server", &program, BINARY_MODE)]).unwrap()
        );
        assert_ne!(
            image.staged.as_file().metadata().unwrap().permissions().mode() & 0o004,
            0
        );
        assert_eq!(
            image.image_bytes,
            image.staged.as_file().metadata().unwrap().len()
        );
    }

    #[test]
    fn abandoned_preparation_removes_its_staged_image() {
        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("program.squashfs");
        let first = stage_executable(&image_path, "/app/server", ARTIFACT_BYTES).unwrap();
        let second = stage_executable(&image_path, "/app/server", ARTIFACT_BYTES).unwrap();
        assert_ne!(first.staged.path(), second.staged.path());
        drop(first);
        drop(second);
        assert!(!image_path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_store_that_will_not_hand_over_the_bytes_is_not_read_as_an_empty_layer() {
        let directory = tempfile::tempdir().unwrap();
        let refusing = mocks::artifacts_refusing(ArtifactError::Transfer("no such key".into()))
            as Arc<dyn ArtifactStore>;
        let error = prepare_layer(&refusing, directory.path(), &layer(|_| {}))
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::Transfer(_)), "{error}");
        assert!(!layer_image_path(directory.path(), &layer(|_| {})).exists());
    }

    #[tokio::test]
    async fn bytes_that_match_are_handed_back_whole_before_anything_is_built_from_them() {
        let store = store(artifact_bytes());
        assert_eq!(
            store
                .read_verified(layer(|_| {}).stored_object().unwrap())
                .await
                .unwrap(),
            artifact_bytes()
        );
    }

    #[tokio::test]
    async fn the_images_come_back_in_the_order_the_document_listed_the_layers() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = crate::ports::MockArtifactStore::new();
        store.expect_read().returning(|key| {
            Ok(if key == &base_layer().stored_object().unwrap().object_key {
                BASE_LAYER_BYTES.to_vec()
            } else {
                ARTIFACT_BYTES.to_vec()
            })
        });
        let images = LayerImages::new(
            Arc::new(store),
            directory.path().to_path_buf(),
            mocks::commands_succeeding().0,
            Arc::new(HostMetrics::new()),
        );
        let prepared = images.prepare(&[base_layer(), layer(|_| {})]).await.unwrap();
        assert_eq!(
            prepared.layer_image_paths,
            vec![
                layer_image_path(directory.path(), &base_layer()),
                layer_image_path(directory.path(), &layer(|_| {})),
            ]
        );
        assert!(prepared.layer_image_paths.iter().all(|path| path.exists()));
        assert_eq!(
            prepared.fetched_bytes,
            (BASE_LAYER_BYTES.len() + ARTIFACT_BYTES.len()) as u64,
            "what was pulled is counted as it was read, layer by layer"
        );

        let again = images.prepare(&[base_layer(), layer(|_| {})]).await.unwrap();
        assert_eq!(again.fetched_bytes, 0, "the cache held both");
    }

    #[test]
    fn every_digest_kind_and_path_gets_its_own_place_in_the_cache() {
        let cache = Path::new("/var/lib/nibrunner/artifacts");
        let packed = layer_image_path(cache, &layer(|_| {}));
        let elsewhere = layer_image_path(cache, &placed_at(layer(|_| {}), "/bin/server"));
        let whole = layer_image_path(cache, &as_filesystem(layer(|_| {})));
        let other = layer_image_path(
            cache,
            &layer(|object| object.digest = Sha256Digest::parse("0".repeat(64)).unwrap()),
        );
        for (a, b) in [(&packed, &elsewhere), (&packed, &whole), (&packed, &other)] {
            assert_ne!(a, b);
        }
        assert_eq!(packed.parent(), whole.parent(), "one digest, one directory");
        assert!(packed.starts_with(cache));
        assert!(whole.ends_with(VERBATIM_IMAGE_FILENAME));
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
            ("/app/server", &artifact_bytes(), BINARY_MODE),
            ("/instance.env", b"NIBRUN_HTTP_PORT=3000\n", CONFIG_MODE),
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
        assert_eq!(mode("/app/server"), BINARY_MODE);
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
        let image_path = prepare_layer(&store, directory.path(), &layer(|_| {}))
            .await
            .unwrap()
            .path;
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&image_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(backdated))
            .unwrap();

        prepare_layer(&store, directory.path(), &layer(|_| {}))
            .await
            .unwrap();
        let touched = std::fs::metadata(&image_path).unwrap().modified().unwrap();
        assert!(touched > backdated, "the image was not touched on a cache hit");
    }
}
