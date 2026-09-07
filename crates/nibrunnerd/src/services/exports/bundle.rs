use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::{DesiredArtifact, TenantEnvironment};

use crate::ports::{ArtifactStore, CommandRequest, CommandRunner, CommandRunnerExt};

const STAGING_MODE: u32 = 0o700;
const DATA_DIRECTORY: &str = "data";
const ENV_FILENAME: &str = ".env";
const ENV_MODE: u32 = 0o600;
const BUNDLE_NAME: &str = "bundle.tar.gz";
const BINARY_MODE: u32 = 0o755;

const MKFS_ROOT_ENTRIES: [&str; 1] = ["lost+found"];

const DUMP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("reading {device_path} produced no files")]
    EmptyDump { device_path: String },
    #[error("{filename} is a path rather than a filename")]
    UnsafeFilename { filename: String },
    #[error("the bundle could not be written: {0}")]
    Unwritable(String),
    #[error("{0}")]
    Artifact(String),
}

impl BundleError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

pub async fn dump_volume(
    commands: &Arc<dyn CommandRunner>,
    device_path: &str,
    staging_dir: &Path,
) -> Result<(), BundleError> {
    let unwritable = |error: std::io::Error| BundleError::Unwritable(error.to_string());
    if let Err(error) = std::fs::remove_dir_all(staging_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(unwritable(error));
        }
    }
    let destination = staging_dir.join(DATA_DIRECTORY);
    crate::json_store::make_directory(&destination, STAGING_MODE)
        .map_err(|error| BundleError::Unwritable(error.to_string()))?;

    let mut request = CommandRequest::new(&[
        "debugfs",
        "-R",
        &format!("rdump / {}", destination.display()),
        device_path,
    ]);
    request.timeout = DUMP_TIMEOUT;
    commands
        .stdout_of(request)
        .await
        .map_err(|error| BundleError::Unwritable(error.message()))?;

    let listed = std::fs::read_dir(&destination).map_err(unwritable)?.count();
    if listed == 0 {
        return Err(BundleError::EmptyDump {
            device_path: device_path.to_string(),
        });
    }
    for entry in MKFS_ROOT_ENTRIES {
        let _ = std::fs::remove_dir_all(destination.join(entry));
    }
    Ok(())
}

pub fn bundle_binary_name(artifact: &DesiredArtifact) -> Result<&str, BundleError> {
    let filename = artifact.filename.as_str();
    let unsafe_name = || BundleError::UnsafeFilename {
        filename: filename.to_string(),
    };
    if filename.is_empty() || filename.starts_with('.') || filename.starts_with('-') {
        return Err(unsafe_name());
    }
    if Path::new(filename).file_name().and_then(|name| name.to_str()) != Some(filename) {
        return Err(unsafe_name());
    }
    Ok(filename)
}

pub fn render_dotenv(environment: &TenantEnvironment) -> String {
    environment
        .iter()
        .map(|(name, value)| format!("{name}={}\n", quoted(value.expose())))
        .collect()
}

fn quoted(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' | '"' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

pub struct WrittenBundle {
    pub path: PathBuf,
    pub size_bytes: u64,
}

pub async fn write_bundle(
    artifacts: &Arc<dyn ArtifactStore>,
    artifact: &DesiredArtifact,
    environment: Option<&TenantEnvironment>,
    staging_dir: &Path,
) -> Result<WrittenBundle, BundleError> {
    let binary_name = bundle_binary_name(artifact)?.to_string();
    let bytes = crate::adapters::vm::artifacts::fetch_verified(artifacts, artifact)
        .await
        .map_err(|error| BundleError::Artifact(error.message()))?;

    let binary_path = staging_dir.join(&binary_name);
    write_file(&binary_path, &bytes, BINARY_MODE)?;

    if let Some(environment) = environment {
        write_file(
            &staging_dir.join(ENV_FILENAME),
            render_dotenv(environment).as_bytes(),
            ENV_MODE,
        )?;
    }

    let bundle_path = staging_dir.join(BUNDLE_NAME);
    archive(&bundle_path, staging_dir, &binary_name, environment.is_some())?;
    let size_bytes = std::fs::metadata(&bundle_path)
        .map_err(|error| BundleError::Unwritable(error.to_string()))?
        .len();
    Ok(WrittenBundle {
        path: bundle_path,
        size_bytes,
    })
}

fn archive(
    bundle_path: &Path,
    staging_dir: &Path,
    binary_name: &str,
    with_environment: bool,
) -> Result<(), BundleError> {
    let unwritable = |error: std::io::Error| BundleError::Unwritable(error.to_string());
    let file = std::fs::File::create(bundle_path).map_err(unwritable)?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder
        .append_dir_all(DATA_DIRECTORY, staging_dir.join(DATA_DIRECTORY))
        .map_err(unwritable)?;
    builder
        .append_path_with_name(staging_dir.join(binary_name), binary_name)
        .map_err(unwritable)?;
    if with_environment {
        builder
            .append_path_with_name(staging_dir.join(ENV_FILENAME), ENV_FILENAME)
            .map_err(unwritable)?;
    }
    builder
        .into_inner()
        .map_err(unwritable)?
        .finish()
        .map_err(unwritable)?;
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), BundleError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|error| BundleError::Unwritable(error.to_string()))?;
    file.write_all(bytes)
        .map_err(|error| BundleError::Unwritable(error.to_string()))?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .map_err(|error| BundleError::Unwritable(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::CommandResult;
    use crate::test_support::mocks;
    use crate::test_support::{artifact, tenant_environment, ARTIFACT_BYTES};

    #[test]
    fn a_filename_that_is_a_path_never_reaches_an_archive_somebody_extracts() {
        assert_eq!(bundle_binary_name(&artifact(|_| {})).unwrap(), "pocketbase");
        for bad in ["../server", "bin/server", ".hidden", "-rf", "", "."] {
            let Ok(filename) = protocol::Filename::parse(bad) else {
                continue;
            };
            let named = artifact(|artifact| artifact.filename = filename);
            assert!(
                bundle_binary_name(&named).is_err(),
                "{bad} was accepted as a name inside a bundle"
            );
        }
    }

    #[test]
    fn a_value_with_a_newline_in_it_stays_on_one_line() {
        let environment = tenant_environment(&[("PLAIN", "value"), ("AWKWARD", "one\ntwo\"three\\four")]);
        let rendered = render_dotenv(&environment);
        assert_eq!(
            rendered,
            "AWKWARD=\"one\\ntwo\\\"three\\\\four\"\nPLAIN=\"value\"\n"
        );
        assert_eq!(rendered.lines().count(), 2);
    }

    #[tokio::test]
    async fn a_dump_that_produced_nothing_is_a_failure_however_debugfs_exited() {
        let root = tempfile::tempdir().unwrap();
        let commands: Arc<dyn CommandRunner> = mocks::commands_succeeding().0;
        let error = dump_volume(&commands, "/dev/nbd63", &root.path().join("staging"))
            .await
            .unwrap_err();
        assert!(matches!(error, BundleError::EmptyDump { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_tenant_who_wrote_nothing_exports_an_empty_data_directory() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        let planted = staging.join(DATA_DIRECTORY).join("lost+found");
        let commands: Arc<dyn CommandRunner> = mocks::commands_answering(move |_| {
            std::fs::create_dir_all(&planted).unwrap();
            Ok(CommandResult::succeeded())
        })
        .0;

        dump_volume(&commands, "/dev/nbd63", &staging).await.unwrap();
        let data = staging.join(DATA_DIRECTORY);
        assert!(data.exists());
        assert!(!data.join("lost+found").exists());
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_bundle_carries_the_data_the_binary_and_the_environment() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir_all(staging.join(DATA_DIRECTORY)).unwrap();
        std::fs::write(staging.join(DATA_DIRECTORY).join("notes.txt"), b"tenant data").unwrap();

        let wanted = artifact(|_| {});
        let artifacts: Arc<dyn ArtifactStore> = mocks::artifacts_holding(ARTIFACT_BYTES);
        let environment = tenant_environment(&[("TOKEN", "hunter2")]);

        let written = write_bundle(&artifacts, &wanted, Some(&environment), &staging)
            .await
            .unwrap();
        assert!(written.size_bytes > 0);
        assert_eq!(
            names_in(&written.path),
            vec![".env", "data/", "data/notes.txt", "pocketbase"]
        );
    }

    #[tokio::test]
    async fn the_binary_in_a_bundle_is_executable() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir_all(staging.join(DATA_DIRECTORY)).unwrap();
        let artifacts: Arc<dyn ArtifactStore> = mocks::artifacts_holding(ARTIFACT_BYTES);
        let written = write_bundle(&artifacts, &artifact(|_| {}), None, &staging)
            .await
            .unwrap();

        let read = std::fs::File::open(&written.path).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(read));
        let binary = archive
            .entries()
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.path().unwrap().display().to_string() == "pocketbase")
            .expect("the bundle carries the binary");
        assert_eq!(binary.header().mode().unwrap() & 0o777, BINARY_MODE);
    }

    #[tokio::test]
    async fn an_app_whose_environment_is_unknown_gets_no_env_file_at_all() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir_all(staging.join(DATA_DIRECTORY)).unwrap();
        let artifacts: Arc<dyn ArtifactStore> = mocks::artifacts_holding(ARTIFACT_BYTES);
        let written = write_bundle(&artifacts, &artifact(|_| {}), None, &staging)
            .await
            .unwrap();
        assert!(!names_in(&written.path).contains(&".env".to_string()));

        let empty = tenant_environment(&[]);
        let with_file = write_bundle(&artifacts, &artifact(|_| {}), Some(&empty), &staging)
            .await
            .unwrap();
        assert!(names_in(&with_file.path).contains(&".env".to_string()));
    }

    fn names_in(bundle: &Path) -> Vec<String> {
        let read = std::fs::File::open(bundle).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(read));
        let mut names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().display().to_string())
            .collect();
        names.sort();
        names
    }
}
