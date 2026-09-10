//! One-time host setup: everything between a machine with the packages on it and a machine
//! `systemctl start nibrunnerd` works on.
//!
//! It is a subcommand rather than something the daemon does on the way up, and that is the whole
//! of its design. Laying ZeroFS down is not the same act as supervising it: there is exactly one
//! read-write `zerofs run` per storage prefix fleet-wide, a second writer is fenced by SlateDB's
//! epoch only after a window of acknowledging writes it then discards, and the thing that holds
//! that lock is a single-instance unit. So this writes the unit and never becomes it.

pub mod prerequisites;
pub mod render;
pub mod service_user;
pub mod zerofs;

use std::path::{Path, PathBuf};

use protocol::HostVersions;

use crate::config::{HostConfig, VolumeBackend};
use crate::json_store::{make_directory, write_text};

const MEBIBYTES_PER_GIBIBYTE: u64 = 1024;

const DIRECTORY_MODE: u32 = 0o700;
const PUBLIC_DIRECTORY_MODE: u32 = 0o755;
const READABLE_FILE_MODE: u32 = 0o644;
const SECRET_FILE_MODE: u32 = 0o600;

const SYSTEMD_DIR: &str = "/etc/systemd/system";

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("this host is not ready:\n{0}")]
    Unready(String),
    #[error("{0}")]
    Refused(String),
}

impl InstallError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// What one run did, so the operator reads the outcome rather than inferring it from silence.
pub struct Laid {
    pub steps: Vec<String>,
}

impl Laid {
    fn note(&mut self, step: impl Into<String>) {
        self.steps.push(step.into());
    }
}

pub async fn run(config: &HostConfig, config_file: &Path, force: bool) -> Result<Laid, InstallError> {
    refuse_unready(config)?;

    let mut laid = Laid { steps: Vec::new() };
    let environment_file = environment_file(config_file);

    for directory in directories(config) {
        make_directory(&directory, DIRECTORY_MODE).map_err(|error| {
            InstallError::Refused(format!("{} could not be made: {error}", directory.display()))
        })?;
    }
    laid.note("directories");

    if let Some(settings) = config.volumes.zerofs() {
        match service_user::ensure(service_user::ZEROFS_USER)? {
            service_user::Made::AlreadyThere => {
                laid.note(format!("{} account already there", service_user::ZEROFS_USER))
            }
            service_user::Made::Created => {
                laid.note(format!("{} account created", service_user::ZEROFS_USER))
            }
        }
        // The cache is the one thing the live server writes that systemd does not create for it,
        // so it is the one thing whose ownership has to be handed over.
        let owner = service_user::ids(service_user::ZEROFS_USER).ok_or_else(|| {
            InstallError::Refused(format!(
                "the {} account could not be read",
                service_user::ZEROFS_USER
            ))
        })?;
        service_user::own(&settings.cache_dir, owner)?;
        laid.note(format!(
            "{} owned by {}",
            settings.cache_dir.display(),
            service_user::ZEROFS_USER
        ));

        match zerofs::ensure(&settings.binary).await? {
            zerofs::Laid::AlreadyThere => laid.note(format!("zerofs {} already installed", zerofs::VERSION)),
            zerofs::Laid::Fetched => laid.note(format!("zerofs {} fetched", zerofs::VERSION)),
        }

        write_generated(
            &settings.config_file,
            &render::zerofs_config(settings, config_file, &environment_file),
            force,
            &mut laid,
        )?;
        write_generated(
            &settings.checkpoint_config_file,
            &render::checkpoint_config(settings, config_file),
            force,
            &mut laid,
        )?;
        write_generated(
            &Path::new(SYSTEMD_DIR).join(render::ZEROFS_UNIT),
            &render::zerofs_unit(settings, config_file, &environment_file),
            force,
            &mut laid,
        )?;
        write_generated(
            &Path::new(SYSTEMD_DIR).join(render::MOUNT_UNIT),
            &render::mount_unit(settings, config_file),
            force,
            &mut laid,
        )?;
    }

    write_generated(
        &Path::new(SYSTEMD_DIR).join(render::DAEMON_DROP_IN),
        &render::daemon_drop_in(config, config_file, &environment_file),
        force,
        &mut laid,
    )?;

    if ensure_environment_file(&environment_file, config)? {
        laid.note(format!(
            "{} created — put this host's secrets in it",
            environment_file.display()
        ));
    }

    stamp_versions(config)?;
    laid.note(format!("{} stamped", config.versions_file.display()));
    Ok(laid)
}

/// Every directory this host writes into that is not created on the way past. `runtime_dir` is
/// deliberately absent: systemd makes it, and one made here would carry the wrong ownership on the
/// first boot after a reboot cleared it.
fn directories(config: &HostConfig) -> Vec<PathBuf> {
    let mut directories = vec![
        config.state_dir.clone(),
        config.snapshot_dir.clone(),
        config.guest_image_dir.clone(),
        config.export_staging_dir.clone(),
    ];
    if let Some(settings) = config.volumes.zerofs() {
        directories.push(settings.cache_dir.clone());
        directories.push(settings.checkpoint_cache_dir.clone());
        directories.push(settings.mount_path.clone());
    }
    directories
}

fn refuse_unready(config: &HostConfig) -> Result<(), InstallError> {
    let checks = prerequisites::check(config);
    let missing: Vec<String> = checks
        .iter()
        .filter(|check| !check.met)
        .map(|check| format!("  {} is missing — {}", check.what, check.remedy))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(InstallError::Unready(missing.join("\n")))
}

/// A file this command wrote is one it may write again. A file without its marker was written by
/// somebody, and replacing it is their decision rather than one a re-run makes for them.
fn write_generated(path: &Path, rendered: &str, force: bool, laid: &mut Laid) -> Result<(), InstallError> {
    match std::fs::read_to_string(path) {
        Ok(existing) if existing == rendered => {
            laid.note(format!("{} unchanged", path.display()));
            return Ok(());
        }
        Ok(existing) if !existing.starts_with(render::GENERATED_MARKER) && !force => {
            return Err(InstallError::Refused(format!(
                "{} was not written by `nibrunnerd install` and will not be replaced by it. Move it aside, or pass --force to overwrite it.",
                path.display()
            )));
        }
        _ => {}
    }
    if let Some(parent) = path.parent() {
        make_directory(parent, PUBLIC_DIRECTORY_MODE).map_err(|error| {
            InstallError::Refused(format!("{} could not be made: {error}", parent.display()))
        })?;
    }
    write_text(path, rendered, READABLE_FILE_MODE).map_err(|error| InstallError::Refused(error.message()))?;
    laid.note(format!("{} written", path.display()));
    Ok(())
}

fn environment_file(config_file: &Path) -> PathBuf {
    config_file
        .parent()
        .unwrap_or(Path::new("/etc/nibrunner"))
        .join(render::ENVIRONMENT_FILENAME)
}

/// Made, never filled. What belongs in it is a password whose loss destroys every tenant disk on
/// this host and a credential that reaches an account this daemon has no business creating things
/// in — so it is written once by whoever holds them, and a re-run never touches it again.
fn ensure_environment_file(path: &Path, config: &HostConfig) -> Result<bool, InstallError> {
    if path.exists() {
        return Ok(false);
    }
    let mut lines = format!(
        "# Read by nibrunnerd and, on a zerofs host, by ZeroFS's own units. Not written by\n\
         # `nibrunnerd install`: everything here is a secret it has no way to know.\n\
         #\n\
         # {}=\n\
         # {}=\n\
         # {}=\n",
        render::REGION_VARIABLE,
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    );
    if matches!(config.volumes, VolumeBackend::Zerofs(_)) {
        lines.push_str(&format!(
            "#\n\
             # Everything in this host's storage prefix is encrypted under this. Losing it loses\n\
             # every tenant disk, so it is permanent for the life of the bucket.\n\
             # {}=\n",
            render::PASSWORD_VARIABLE
        ));
    }
    write_text(path, &lines, SECRET_FILE_MODE).map_err(|error| InstallError::Refused(error.message()))?;
    Ok(true)
}

/// The file `paths.versions_file` was always for: what the installer laid down, read back by the
/// daemon so a report names the host rather than the build.
fn stamp_versions(config: &HostConfig) -> Result<(), InstallError> {
    let versions = HostVersions {
        agent: env!("CARGO_PKG_VERSION").to_string(),
        guest_image: prerequisites::guest_image(config).map_err(InstallError::Refused)?,
        zerofs: match config.volumes {
            VolumeBackend::LocalFile => "none".to_string(),
            VolumeBackend::Zerofs(_) => zerofs::VERSION.to_string(),
        },
        firecracker: crate::adapters::vm::process::FIRECRACKER_VERSION.to_string(),
    };
    crate::json_store::write_json(&config.versions_file, &versions)
        .map_err(|error| InstallError::Refused(error.message()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn laid() -> Laid {
        Laid { steps: Vec::new() }
    }

    #[test]
    fn a_file_this_command_wrote_is_one_it_writes_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let first = format!("{}\nfirst\n", render::GENERATED_MARKER);
        let second = format!("{}\nsecond\n", render::GENERATED_MARKER);

        write_generated(&path, &first, false, &mut laid()).unwrap();
        write_generated(&path, &second, false, &mut laid()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), second);
    }

    #[test]
    fn a_file_somebody_else_wrote_is_refused_by_name_rather_than_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "# mine\ndisk_size_gb = 9000\n").unwrap();

        let error = write_generated(&path, "# theirs\n", false, &mut laid()).unwrap_err();
        assert!(error.message().contains("--force"), "{}", error.message());
        assert!(error.message().contains(&path.display().to_string()));
        assert!(std::fs::read_to_string(&path).unwrap().contains("9000"));
    }

    #[test]
    fn force_is_what_replaces_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "# mine\n").unwrap();
        write_generated(&path, "# theirs\n", true, &mut laid()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# theirs\n");
    }

    // Re-running is how a host is brought up to a new release, so it has to be readable as having
    // done nothing when there was nothing to do.
    #[test]
    fn a_rerun_that_changes_nothing_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let rendered = format!("{}\nsame\n", render::GENERATED_MARKER);
        let mut steps = laid();
        write_generated(&path, &rendered, false, &mut steps).unwrap();
        write_generated(&path, &rendered, false, &mut steps).unwrap();
        assert!(steps.steps[0].ends_with("written"), "{:?}", steps.steps);
        assert!(steps.steps[1].ends_with("unchanged"), "{:?}", steps.steps);
    }

    #[test]
    fn the_environment_file_sits_beside_the_configuration_that_named_it() {
        assert_eq!(
            environment_file(Path::new("/etc/nibrunner/config.toml")),
            PathBuf::from("/etc/nibrunner/host.env")
        );
        assert_eq!(
            environment_file(Path::new("/srv/other/host.toml")),
            PathBuf::from("/srv/other/host.env")
        );
    }

    #[test]
    fn a_secret_file_is_made_once_and_never_written_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.env");
        let config = HostConfig::under(Path::new("/srv/nibrunner"));

        assert!(ensure_environment_file(&path, &config).unwrap());
        std::fs::write(&path, "AWS_ACCESS_KEY_ID=real\n").unwrap();
        assert!(!ensure_environment_file(&path, &config).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "AWS_ACCESS_KEY_ID=real\n"
        );
    }

    #[test]
    fn the_secret_file_is_readable_only_by_the_host_that_runs_on_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.env");
        ensure_environment_file(&path, &HostConfig::under(Path::new("/srv/nibrunner"))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, SECRET_FILE_MODE);
        }
    }

    #[test]
    fn a_local_file_host_is_told_about_no_password_it_will_never_use() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.env");
        ensure_environment_file(&path, &HostConfig::under(Path::new("/srv/nibrunner"))).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains(render::PASSWORD_VARIABLE), "{written}");
        assert!(written.contains(render::REGION_VARIABLE), "{written}");
    }

    #[test]
    fn a_zerofs_host_is_told_what_the_password_costs_to_lose() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.env");
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        config.volumes = VolumeBackend::Zerofs(Box::new(crate::test_support::zerofs_settings(|_| {})));
        ensure_environment_file(&path, &config).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains(render::PASSWORD_VARIABLE), "{written}");
    }

    #[test]
    fn a_local_file_host_is_given_no_directory_only_zerofs_would_want() {
        let config = HostConfig::under(Path::new("/srv/nibrunner"));
        let directories = directories(&config);
        assert!(directories.contains(&config.snapshot_dir));
        assert!(
            !directories.iter().any(|path| path.ends_with("zerofs")),
            "{directories:?}"
        );
        // systemd owns it, and one made here comes back with the wrong owner after a reboot.
        assert!(!directories.contains(&config.runtime_dir), "{directories:?}");
    }

    #[test]
    fn a_zerofs_host_gets_the_caches_and_the_mount_point() {
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        let settings = crate::test_support::zerofs_settings(|_| {});
        config.volumes = VolumeBackend::Zerofs(Box::new(settings.clone()));
        let directories = directories(&config);
        for wanted in [
            &settings.cache_dir,
            &settings.checkpoint_cache_dir,
            &settings.mount_path,
        ] {
            assert!(directories.contains(wanted), "{directories:?}");
        }
    }

    #[test]
    fn an_unready_host_is_told_everything_that_is_missing_at_once() {
        let mut config = HostConfig::under(Path::new("/nibrunner-nowhere"));
        config.volumes = VolumeBackend::Zerofs(Box::new(crate::test_support::zerofs_settings(|_| {})));
        let Err(error) = refuse_unready(&config) else {
            // A host that genuinely has all of this is a Linux box with the tools on it, and the
            // guest image check still fails against a directory that is not there.
            panic!("a directory that does not exist cannot be a ready host");
        };
        assert!(error.message().contains("guest image"), "{}", error.message());
    }
}
