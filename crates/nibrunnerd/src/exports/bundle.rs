//! What an export hands over: the tenant's data, the binary that was running on it, and the
//! environment it was running under.
//!
//! Ported from `apps/agent/src/lib/exports/bundle.ts`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::{DesiredArtifact, TenantEnvironment};

use crate::services::{ArtifactStore, CommandRequest, CommandRunner};

const STAGING_MODE: u32 = 0o700;
const DATA_DIRECTORY: &str = "data";
const ENV_FILENAME: &str = ".env";
/// The tenant's environment in the clear, which is what it is for and why nobody else may read it.
const ENV_MODE: u32 = 0o600;
const BUNDLE_NAME: &str = "bundle.tar.gz";
const BINARY_MODE: u32 = 0o755;

/// What `mke2fs` puts at the root of every filesystem it writes. A tenant did not create these and
/// has no use for them, so an export does not show them.
///
/// **At the root only.** `lost+found` is reserved by the filesystem in exactly one directory; the
/// same name deeper in the tree is a directory a tenant made, and hiding it would be hiding their
/// own data from them.
const MKFS_ROOT_ENTRIES: [&str; 1] = ["lost+found"];

/// A tenant filesystem is unbounded, and a shorter ceiling would abort a large export part-way.
///
/// An hour is a number that can actually be reached. It does not sit under the guest's own freeze
/// ceiling: the read runs against a checkpoint with nobody frozen behind it, so the two bound
/// different things — that one the cut, this one the read — rather than being two answers to the
/// same question.
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

/// Read with `debugfs`, which walks inodes in userspace: the bundle is built from a filesystem the
/// host never asks its kernel to interpret. Mounting it — even read-only — would give that up.
///
/// `debugfs` reports a failed `rdump` on stderr and still exits 0, so an empty destination is the
/// only reliable signal that nothing came out.
pub async fn dump_volume(
    commands: &Arc<dyn CommandRunner>,
    device_path: &str,
    staging_dir: &Path,
) -> Result<(), BundleError> {
    let unwritable = |error: std::io::Error| BundleError::Unwritable(error.to_string());
    // Removed first: a staging tree that outlived a kill is a tenant's dataset in the clear, and
    // an export that added to one would hand over somebody else's files.
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
    // Dropped after the emptiness check and never before it: a filesystem holding nothing but
    // `lost+found` is a tenant who has written no data, and removing it first would report that
    // as a dump that failed.
    for entry in MKFS_ROOT_ENTRIES {
        let _ = std::fs::remove_dir_all(destination.join(entry));
    }
    Ok(())
}

/// Re-checked rather than trusted from the wire, and refused rather than corrected: this becomes a
/// path inside an archive somebody extracts on their own machine, and the schema already
/// constrains it, so anything else came from a peer that did not honour the contract.
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

/// Quoted, where the config drive's `instance.env` refuses a value it cannot represent instead.
/// The two have different readers: that one is parsed by an init with no parser, so a value it
/// cannot carry is an instance that must not boot, while this one is read by whatever the owner
/// runs the binary under next — and an export is the last thing that may fail on a value somebody
/// set. So the escaping is dotenv's, and a newline becomes `\n` rather than the end of the line.
pub fn render_dotenv(environment: &TenantEnvironment) -> String {
    // Already ordered: the environment is a sorted map, so a bundle written twice is the same
    // bundle twice.
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

/// The binary is fetched from the artifact store rather than lifted out of the local squashfs
/// cache, because the download proves the digest on the way past.
pub async fn write_bundle(
    artifacts: &Arc<dyn ArtifactStore>,
    artifact: &DesiredArtifact,
    environment: Option<&TenantEnvironment>,
    staging_dir: &Path,
) -> Result<WrittenBundle, BundleError> {
    let binary_name = bundle_binary_name(artifact)?.to_string();
    let bytes = crate::vm::artifacts::fetch_verified(artifacts, artifact)
        .await
        .map_err(|error| BundleError::Artifact(error.message()))?;

    let binary_path = staging_dir.join(&binary_name);
    // A transfer writes what a transfer writes, and an archive records the mode it finds. Without
    // this the bundle carries a binary the owner has to chmod before the copy they were handed
    // will run.
    write_file(&binary_path, &bytes, BINARY_MODE)?;

    // An empty file for an app that set no variables, and no file at all when the control plane
    // could not say what it was configured with: the first is an answer, and the second would be
    // an empty file pretending to be one.
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

/// Named entries rather than the whole directory, which would sweep the archive into itself.
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
    // Names come from a tenant's own filenames, so they are carried as data rather than through a
    // shell: nothing here interprets them.
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
    // Set again, because the mode above only applies to a file this call created and an export
    // that reran over its own staging tree would otherwise keep whatever was there.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .map_err(|error| BundleError::Unwritable(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{CommandResult, RecordingCommandRunner, StubArtifactStore};
    use crate::test_support::{artifact, tenant_environment, ARTIFACT_BYTES};

    /// The schema constrains this already, so anything that reaches here came from a peer that did
    /// not honour the contract — and it becomes a path inside an archive somebody extracts.
    #[test]
    fn a_filename_that_is_a_path_never_reaches_an_archive_somebody_extracts() {
        assert_eq!(bundle_binary_name(&artifact(|_| {})).unwrap(), "pocketbase");
        for bad in ["../server", "bin/server", ".hidden", "-rf", "", "."] {
            let Ok(filename) = protocol::Filename::parse(bad) else {
                // Refused a layer earlier, which is where it should be refused.
                continue;
            };
            let named = artifact(|artifact| artifact.filename = filename);
            assert!(
                bundle_binary_name(&named).is_err(),
                "{bad} was accepted as a name inside a bundle"
            );
        }
    }

    /// Read by whatever the owner runs the binary under next, so a value nobody could represent is
    /// an export that fails on something somebody set rather than one that hands it over.
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

    /// `debugfs` reports a failed `rdump` on stderr and exits 0, so nothing coming out is the only
    /// reliable signal that it did not work.
    #[tokio::test]
    async fn a_dump_that_produced_nothing_is_a_failure_however_debugfs_exited() {
        let root = tempfile::tempdir().unwrap();
        let commands: Arc<dyn CommandRunner> = RecordingCommandRunner::succeeding();
        let error = dump_volume(&commands, "/dev/nbd63", &root.path().join("staging"))
            .await
            .unwrap_err();
        assert!(matches!(error, BundleError::EmptyDump { .. }), "{error}");
    }

    /// A filesystem holding nothing but `lost+found` is a tenant who has written no data, so it is
    /// dropped after the emptiness check and never before it — otherwise that reads as a failure.
    #[tokio::test]
    async fn a_tenant_who_wrote_nothing_exports_an_empty_data_directory() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        let planted = staging.join(DATA_DIRECTORY).join("lost+found");
        // Standing in for `debugfs`: what it leaves behind for a filesystem nobody has written to.
        let commands: Arc<dyn CommandRunner> = RecordingCommandRunner::answering(move |_| {
            std::fs::create_dir_all(&planted).unwrap();
            Ok(CommandResult::succeeded())
        });

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
        let artifacts: Arc<dyn ArtifactStore> = StubArtifactStore::holding(ARTIFACT_BYTES);
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

    /// A transfer writes what a transfer writes, and without this the owner has to chmod the copy
    /// they were handed before it will run.
    #[tokio::test]
    async fn the_binary_in_a_bundle_is_executable() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir_all(staging.join(DATA_DIRECTORY)).unwrap();
        let artifacts: Arc<dyn ArtifactStore> = StubArtifactStore::holding(ARTIFACT_BYTES);
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

    /// An empty file is an answer; no file at all is the control plane not having said. The two
    /// must not read the same to whoever opens the bundle.
    #[tokio::test]
    async fn an_app_whose_environment_is_unknown_gets_no_env_file_at_all() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir_all(staging.join(DATA_DIRECTORY)).unwrap();
        let artifacts: Arc<dyn ArtifactStore> = StubArtifactStore::holding(ARTIFACT_BYTES);
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
