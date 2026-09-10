//! One-time host setup: everything between a machine with the packages on it and a machine
//! `systemctl start nibrunnerd` works on.
//!
//! It is a subcommand rather than something the daemon does on the way up, and that is the whole
//! of its design. Laying ZeroFS down is not the same act as supervising it: there is exactly one
//! read-write `zerofs run` per storage prefix fleet-wide, a second writer is fenced by SlateDB's
//! epoch only after a window of acknowledging writes it then discards, and the thing that holds
//! that lock is a single-instance unit. So this writes the unit and never becomes it.

pub mod guest_image;
pub mod kernel;
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

pub async fn run(
    config: &HostConfig,
    config_file: &Path,
    force: bool,
    release: Option<&Path>,
) -> Result<Laid, InstallError> {
    let mut laid = Laid { steps: Vec::new() };

    // Before the checks rather than after them, because the guest image is one of the things they
    // refuse a host for — and a release was named precisely so this host would not have to have it
    // already.
    if let Some(release) = release {
        match guest_image::ensure(&config.guest_image_dir, release)? {
            guest_image::Laid::AlreadyThere(version) => {
                laid.note(format!("guest image {version} already there"))
            }
            guest_image::Laid::Taken(version) => laid.note(format!("guest image {version} laid down")),
        }
    }
    refuse_unready(config)?;

    let environment_file = environment_file(config_file);

    // Written before they are applied, so a host that has to reboot to finish reboots into the
    // settings rather than back out of them.
    for (path, rendered) in kernel::files(config, config_file) {
        write_generated(&path, &rendered, force, &mut laid)?;
    }
    kernel::apply(config, &mut laid)?;

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

    if let Ok(binary) = std::env::current_exe() {
        write_generated(
            &Path::new(SYSTEMD_DIR).join(render::DAEMON_UNIT),
            &render::daemon_unit(config_file, &binary),
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

/// What a person still has to do, said by the thing that knows: which units this host has, where
/// its environment file is, and whether anything needs to go in it at all. Nothing that bootstraps
/// this binary should be repeating any of it.
pub fn what_is_left(config: &HostConfig, config_file: &Path) -> String {
    let environment_file = environment_file(config_file);
    let mut said = String::from("\nThis host is laid out. What is left:\n");
    if reaches_an_object_store(config) || matches!(config.volumes, VolumeBackend::Zerofs(_)) {
        said.push_str(&format!(
            "  the secrets in {}, which nothing but you holds\n",
            environment_file.display()
        ));
    }
    said.push_str("  systemctl daemon-reload\n");
    if config.volumes.zerofs().is_some() {
        said.push_str(&format!(
            "  systemctl enable --now {} {}\n",
            render::ZEROFS_UNIT,
            render::MOUNT_UNIT
        ));
    }
    said.push_str("  systemctl enable --now nibrunnerd\n");
    said
}

/// Said only to a host laid out from the starting point this binary carries: it is serving, and
/// every choice about what it serves and where its storage is is still ahead of whoever ran this.
pub fn what_is_still_yours(config_file: &Path) -> String {
    format!(
        "\nIt is running the configuration this binary carries: volumes as files on its own disk,\n\
         no proxy, no object store. To make it this host's —\n\
         \n  \
         edit {}\n  \
         then `nibrunnerd install` again, and `systemctl restart nibrunnerd`\n\
         \n\
         docs/config.md is every key in it.\n",
        config_file.display()
    )
}

/// The smallest configuration this daemon accepts, from the one copy of it in this repository.
pub fn write_starter_configuration(path: &Path) -> Result<(), crate::json_store::StoreError> {
    const STARTER: &str = include_str!("../../../../deploy/config.toml");
    write_text(path, STARTER, READABLE_FILE_MODE)
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

/// What this command cannot put right: a machine without hardware virtualisation, a tool it does
/// not install, a guest image nobody laid down. The kernel settings used to be in here and are not
/// any more — those it sets.
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
///
/// It is made even when this host needs nothing in it: the drop-in names it with a bare
/// `EnvironmentFile=`, and systemd fails a unit whose environment file is not there.
fn ensure_environment_file(path: &Path, config: &HostConfig) -> Result<bool, InstallError> {
    if path.exists() {
        return Ok(false);
    }
    let mut lines = "# Read by nibrunnerd and, on a zerofs host, by ZeroFS's own units. Not written by\n\
                     # `nibrunnerd install`: everything here is a secret it has no way to know.\n"
        .to_string();
    if reaches_an_object_store(config) {
        lines.push_str(&format!(
            "#\n\
             # This host reaches an object store, and resolves credentials from this environment.\n\
             # {REGION}=\n\
             # {KEY}=\n\
             # {SECRET}=\n",
            REGION = render::REGION_VARIABLE,
            KEY = "AWS_ACCESS_KEY_ID",
            SECRET = "AWS_SECRET_ACCESS_KEY",
        ));
    }
    if matches!(config.volumes, VolumeBackend::Zerofs(_)) {
        lines.push_str(&format!(
            "#\n\
             # Everything in this host's storage prefix is encrypted under this. Losing it loses\n\
             # every tenant disk, so it is permanent for the life of the bucket.\n\
             # {}=\n",
            render::PASSWORD_VARIABLE
        ));
    }
    if lines.lines().all(|line| !line.ends_with('=')) {
        lines.push_str(
            "#\n\
             # Nothing on this host needs a secret: its volumes are files on its own disk and its\n\
             # stores are directories on it. This file is here because the unit names it.\n",
        );
    }
    write_text(path, &lines, SECRET_FILE_MODE).map_err(|error| InstallError::Refused(error.message()))?;
    Ok(true)
}

/// Whether anything this host is configured to reach is an object store rather than a directory.
/// A host whose artifacts, exports and volumes are all local needs no credential, and naming one
/// in its environment file would be telling it to go and find something it does not have.
fn reaches_an_object_store(config: &HostConfig) -> bool {
    let remote = |url: &str| url.starts_with("s3://");
    remote(&config.artifact_store_url)
        || remote(&config.export_store_url)
        || config
            .volumes
            .zerofs()
            .is_some_and(|settings| remote(&settings.storage_url))
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

    // It is written to a host that then has to load it. A starting point this daemon refuses is
    // one that leaves a fresh machine no better off than having no configuration at all.
    #[test]
    fn the_configuration_this_binary_carries_is_one_it_would_accept() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        write_starter_configuration(&path).unwrap();
        let config = HostConfig::from_file(&path).expect("the starting configuration must load");
        assert!(matches!(config.volumes, VolumeBackend::LocalFile));
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

    // Its volumes are files on its own disk and its stores are directories on it, so there is no
    // account for it to reach and nothing to encrypt. Naming either would send an operator looking
    // for a credential this host has no use for.
    #[test]
    fn a_host_that_reaches_nothing_remote_is_asked_for_no_secret_at_all() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.env");
        ensure_environment_file(&path, &HostConfig::under(Path::new("/srv/nibrunner"))).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains(render::PASSWORD_VARIABLE), "{written}");
        assert!(!written.contains(render::REGION_VARIABLE), "{written}");
        assert!(!written.contains("AWS_ACCESS_KEY_ID"), "{written}");
        assert!(
            written.contains("Nothing on this host needs a secret"),
            "{written}"
        );
    }

    // The file is made whether or not it holds anything, because the drop-in names it with a bare
    // EnvironmentFile= and systemd fails a unit whose environment file is not there.
    #[test]
    fn a_host_that_needs_no_secret_still_gets_the_file_its_unit_names() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.env");
        assert!(ensure_environment_file(&path, &HostConfig::under(Path::new("/srv/nibrunner"))).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn a_local_file_host_whose_artifacts_are_in_a_bucket_is_asked_for_the_credential() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.env");
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        config.artifact_store_url = "s3://nibrunner-artifacts/artifacts".to_string();
        ensure_environment_file(&path, &config).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("AWS_ACCESS_KEY_ID"), "{written}");
        // Its volumes are still files on its own disk, so there is still nothing to encrypt.
        assert!(!written.contains(render::PASSWORD_VARIABLE), "{written}");
    }

    #[test]
    fn every_store_a_host_could_reach_remotely_is_looked_at() {
        let local = HostConfig::under(Path::new("/srv/nibrunner"));
        assert!(!reaches_an_object_store(&local));

        let mut exports = HostConfig::under(Path::new("/srv/nibrunner"));
        exports.export_store_url = "s3://nibrunner-exports/exports".to_string();
        assert!(reaches_an_object_store(&exports));

        let mut volumes = HostConfig::under(Path::new("/srv/nibrunner"));
        volumes.volumes = VolumeBackend::Zerofs(Box::new(crate::test_support::zerofs_settings(|settings| {
            settings.storage_url = "s3://nibrunner-filesystems/hetzner-1".to_string();
        })));
        assert!(reaches_an_object_store(&volumes));
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
