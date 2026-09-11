use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::TenantEnvironment;

use crate::ports::{CommandRequest, CommandRunner, CommandRunnerExt};

const STAGING_MODE: u32 = 0o700;
const DATA_DIRECTORY: &str = "data";
const ENV_FILENAME: &str = ".env";
const ENV_MODE: u32 = 0o600;
const BUNDLE_NAME: &str = "bundle.tar.gz";

const MKFS_ROOT_ENTRIES: [&str; 1] = ["lost+found"];

const DUMP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("reading {device_path} produced no files")]
    EmptyDump { device_path: String },
    #[error("the bundle could not be written: {0}")]
    Unwritable(String),
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

pub fn write_bundle(
    environment: Option<&TenantEnvironment>,
    staging_dir: &Path,
) -> Result<WrittenBundle, BundleError> {
    if let Some(environment) = environment {
        write_file(
            &staging_dir.join(ENV_FILENAME),
            render_dotenv(environment).as_bytes(),
            ENV_MODE,
        )?;
    }

    let bundle_path = staging_dir.join(BUNDLE_NAME);
    archive(&bundle_path, staging_dir, environment.is_some())?;
    let size_bytes = std::fs::metadata(&bundle_path)
        .map_err(|error| BundleError::Unwritable(error.to_string()))?
        .len();
    Ok(WrittenBundle {
        path: bundle_path,
        size_bytes,
    })
}

fn archive(bundle_path: &Path, staging_dir: &Path, with_environment: bool) -> Result<(), BundleError> {
    let unwritable = |error: std::io::Error| BundleError::Unwritable(error.to_string());
    let file = std::fs::File::create(bundle_path).map_err(unwritable)?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder
        .append_dir_all(DATA_DIRECTORY, staging_dir.join(DATA_DIRECTORY))
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
    use crate::test_support::tenant_environment;

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

    #[test]
    fn a_bundle_carries_the_data_and_the_environment_and_nothing_the_layers_already_hold() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir_all(staging.join(DATA_DIRECTORY)).unwrap();
        std::fs::write(staging.join(DATA_DIRECTORY).join("notes.txt"), b"tenant data").unwrap();
        let environment = tenant_environment(&[("TOKEN", "hunter2")]);

        let written = write_bundle(Some(&environment), &staging).unwrap();
        assert!(written.size_bytes > 0);
        assert_eq!(names_in(&written.path), vec![".env", "data/", "data/notes.txt"]);
    }

    #[test]
    fn an_app_whose_environment_is_unknown_gets_no_env_file_at_all() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir_all(staging.join(DATA_DIRECTORY)).unwrap();
        let written = write_bundle(None, &staging).unwrap();
        assert!(!names_in(&written.path).contains(&".env".to_string()));

        let empty = tenant_environment(&[]);
        let with_file = write_bundle(Some(&empty), &staging).unwrap();
        assert!(names_in(&with_file.path).contains(&".env".to_string()));
    }

    #[tokio::test]
    async fn a_dump_the_tool_refused_is_not_reported_as_an_empty_volume() {
        let root = tempfile::tempdir().unwrap();
        let commands: Arc<dyn CommandRunner> = mocks::commands_answering(|_| {
            Err(crate::ports::CommandError::Failed {
                executable: "debugfs".into(),
                code: 1,
                reason: ": no such device".into(),
            })
        })
        .0;
        let error = dump_volume(&commands, "/dev/nbd63", &root.path().join("staging"))
            .await
            .unwrap_err();
        assert!(matches!(error, BundleError::Unwritable(_)), "{error}");
        assert!(error.message().contains("no such device"), "{error}");
    }

    #[tokio::test]
    async fn what_the_last_export_left_behind_is_cleared_before_this_one_reads() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        let leftover = staging.join(DATA_DIRECTORY).join("someone-elses.txt");
        std::fs::create_dir_all(leftover.parent().unwrap()).unwrap();
        std::fs::write(&leftover, b"an earlier tenant").unwrap();

        let planted = staging.join(DATA_DIRECTORY).join("notes.txt");
        let commands: Arc<dyn CommandRunner> = mocks::commands_answering(move |_| {
            std::fs::write(&planted, b"this tenant").unwrap();
            Ok(CommandResult::succeeded())
        })
        .0;

        dump_volume(&commands, "/dev/nbd63", &staging).await.unwrap();
        assert!(!leftover.exists());
        assert_eq!(
            std::fs::read_dir(staging.join(DATA_DIRECTORY)).unwrap().count(),
            1
        );
    }

    #[tokio::test]
    async fn the_device_a_dump_read_from_is_the_one_the_caller_named() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        let planted = staging.join(DATA_DIRECTORY).join("notes.txt");
        let (commands, log) = mocks::commands_answering(move |_| {
            std::fs::write(&planted, b"tenant data").unwrap();
            Ok(CommandResult::succeeded())
        });
        let commands: Arc<dyn CommandRunner> = commands;

        dump_volume(&commands, "/dev/nbd63", &staging).await.unwrap();
        let asked = log.commands();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0][0], "debugfs");
        assert_eq!(asked[0].last().unwrap(), "/dev/nbd63");
        assert!(asked[0][2].starts_with("rdump / "));
        assert!(asked[0][2].ends_with(&staging.join(DATA_DIRECTORY).display().to_string()));
    }

    #[test]
    fn a_bundle_that_cannot_be_written_is_a_failure_rather_than_an_empty_archive() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        assert!(write_bundle(None, &staging).is_err());
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
