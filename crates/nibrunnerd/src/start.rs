//! `nibrunnerd start`: the host `install` laid out, asked of systemd. Everything before this point
//! writes files; this is the one place that asks for them to be run, and it asks only after
//! checking what a start would otherwise find out too late and say from too far away — a secret
//! still empty in `host.env`, a certificate the configuration names that is not there. With those
//! checked, a start that fails is one the configuration got wrong.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::HostConfig;
use crate::install::render::{DAEMON_UNIT, MOUNT_UNIT, ZEROFS_UNIT};
use crate::install::{secrets, unit_path, Laid};

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("this host cannot be started:\n{0}\nnothing was started")]
    Unstartable(String),
    #[error("{0}")]
    Systemd(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Started,
    Restarted,
    /// Left as it was, because nothing it reads changed.
    Running,
    Failed,
}

impl Outcome {
    pub fn said(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Restarted => "restarted",
            Self::Running => "running",
            Self::Failed => "failed",
        }
    }
}

pub struct Report {
    pub units: Vec<(&'static str, Outcome)>,
}

impl Report {
    pub fn all_up(&self) -> bool {
        self.units.iter().all(|(_, outcome)| *outcome != Outcome::Failed)
    }

    /// One command that shows every unit this host has, because the one that failed is rarely
    /// the one that says why: the mount fails because the server it requires did.
    pub fn journal(&self) -> String {
        let named: Vec<String> = self.units.iter().map(|(unit, _)| format!("-u {unit}")).collect();
        format!("journalctl {} -n 60 --no-pager", named.join(" "))
    }
}

pub fn run(config: &HostConfig, environment_file: &Path, laid: &Laid) -> Result<Report, StartError> {
    refuse_unstartable(config, environment_file)?;
    must(&["daemon-reload"])?;

    let plan = plan(config, laid);
    let mut enable = vec!["enable"];
    enable.extend(plan.iter().map(|(unit, _)| *unit));
    must(&enable)?;

    let mut units = Vec::new();
    for (unit, restart) in plan {
        let was_active = is_active(unit);
        let verb = if restart { "restart" } else { "start" };
        let outcome = match systemctl(&[verb, unit])? {
            true if !is_active(unit) => Outcome::Failed,
            true if !was_active => Outcome::Started,
            true if restart => Outcome::Restarted,
            true => Outcome::Running,
            false => Outcome::Failed,
        };
        units.push((unit, outcome));
    }
    // `Type=exec` calls a unit started the moment it execs, and one that exits right after is
    // read as started by the check above — and as auto-restarting by this one, once the units
    // after it have taken their time coming up.
    for (unit, outcome) in &mut units {
        if *outcome != Outcome::Failed && !is_active(unit) {
            *outcome = Outcome::Failed;
        }
    }
    Ok(Report { units })
}

/// Which units this host has, in the order they come up, and whether each is restarted or merely
/// started. The daemon always: it reads `config.toml` itself, so an edit there changes no rendered
/// file, and its restart takes no tenant with it. ZeroFS only when something it reads changed:
/// its restart drops every NBD device under every live guest, and the mount goes with it.
fn plan(config: &HostConfig, laid: &Laid) -> Vec<(&'static str, bool)> {
    let mut plan = Vec::new();
    if let Some(settings) = config.volumes.zerofs() {
        let rewritten = |paths: &[PathBuf]| laid.written.iter().any(|path| paths.contains(path));
        let zerofs = rewritten(&[
            settings.binary.clone(),
            settings.config_file.clone(),
            unit_path(ZEROFS_UNIT),
        ]);
        let mount = zerofs || rewritten(&[unit_path(MOUNT_UNIT)]);
        plan.push((ZEROFS_UNIT, zerofs));
        plan.push((MOUNT_UNIT, mount));
    }
    plan.push((DAEMON_UNIT, true));
    plan
}

fn refuse_unstartable(config: &HostConfig, environment_file: &Path) -> Result<(), StartError> {
    let mut reasons: Vec<String> = secrets::missing(config, environment_file)
        .into_iter()
        .map(|name| format!("  {name} is not set in {}", environment_file.display()))
        .collect();
    for (what, path) in tls_material(config) {
        if !path.is_file() {
            reasons.push(format!(
                "  [proxy.http.tls] {what} {} is not there",
                path.display()
            ));
        }
    }
    if reasons.is_empty() {
        return Ok(());
    }
    Err(StartError::Unstartable(reasons.join("\n")))
}

fn tls_material(config: &HostConfig) -> Vec<(&'static str, &Path)> {
    let Some(tls) = config.proxy.http.as_ref().and_then(|http| http.tls.as_ref()) else {
        return Vec::new();
    };
    let mut material = vec![
        ("certificate", tls.certificate.as_path()),
        ("key", tls.key.as_path()),
    ];
    if let Some(client_ca) = &tls.client_ca {
        material.push(("client_ca.certificate", client_ca.as_path()));
    }
    material
}

/// Longer than any unit here can take to start or stop — the mount waits up to twenty seconds,
/// ZeroFS is given two minutes to stop — so reaching it means a job that is not going to finish,
/// which is a thing to be told about rather than waited on.
const SYSTEMCTL_DEADLINE: Duration = Duration::from_secs(300);

fn must(arguments: &[&str]) -> Result<(), StartError> {
    if systemctl(arguments)? {
        return Ok(());
    }
    Err(StartError::Systemd(format!(
        "systemctl {} failed",
        arguments.join(" ")
    )))
}

/// Said before it is run, so a call that blocks is seen blocking on the command it is. systemd's
/// own stderr comes straight through: "Job for X failed, see journalctl -xeu X" is the message,
/// and rewording it would only lose the unit's name.
fn systemctl(arguments: &[&str]) -> Result<bool, StartError> {
    println!("  systemctl {}", arguments.join(" "));
    let could_not =
        |error: std::io::Error| StartError::Systemd(format!("systemctl could not be run: {error}"));
    let mut child = Command::new("systemctl")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(could_not)?;
    let deadline = Instant::now() + SYSTEMCTL_DEADLINE;
    loop {
        if let Some(status) = child.try_wait().map_err(could_not)? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(StartError::Systemd(format!(
                "systemctl {} has not returned after {} minutes. The job is still queued: `systemctl list-jobs` shows it, and `systemctl status` on the unit says what it is waiting on",
                arguments.join(" "),
                SYSTEMCTL_DEADLINE.as_secs() / 60
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HttpListener, TlsMaterial, VolumeBackend};
    use crate::test_support::zerofs_settings;

    fn zerofs_host() -> HostConfig {
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        config.volumes = VolumeBackend::Zerofs(Box::new(zerofs_settings(|_| {})));
        config
    }

    fn written(paths: &[PathBuf]) -> Laid {
        Laid {
            done: Vec::new(),
            written: paths.to_vec(),
        }
    }

    #[test]
    fn a_local_file_host_has_one_unit_and_it_is_always_restarted() {
        let plan = plan(&HostConfig::under(Path::new("/srv/nibrunner")), &Laid::default());
        assert_eq!(plan, [(DAEMON_UNIT, true)]);
    }

    #[test]
    fn zerofs_is_left_running_when_nothing_it_reads_changed() {
        let plan = plan(
            &zerofs_host(),
            &written(&[unit_path("nibrunnerd.service.d/install.conf")]),
        );
        assert_eq!(
            plan,
            [(ZEROFS_UNIT, false), (MOUNT_UNIT, false), (DAEMON_UNIT, true)]
        );
    }

    // Its config, its unit and its binary are what it reads on the way up. A restart drops every
    // device under every guest, so each is named rather than "anything at all changed".
    #[test]
    fn zerofs_is_restarted_when_what_it_reads_was_rewritten_and_the_mount_follows() {
        let config = zerofs_host();
        let settings = config.volumes.zerofs().unwrap();
        for changed in [
            settings.config_file.clone(),
            settings.binary.clone(),
            unit_path(ZEROFS_UNIT),
        ] {
            let plan = plan(&config, &written(std::slice::from_ref(&changed)));
            assert_eq!(
                plan,
                [(ZEROFS_UNIT, true), (MOUNT_UNIT, true), (DAEMON_UNIT, true)],
                "{}",
                changed.display()
            );
        }
        // Read by the checkpoint servers the daemon spawns, never by the live server.
        let plan = plan(
            &config,
            &written(std::slice::from_ref(&settings.checkpoint_config_file)),
        );
        assert_eq!(plan[0], (ZEROFS_UNIT, false));
    }

    #[test]
    fn the_mount_alone_is_restarted_when_only_its_unit_changed() {
        let plan = plan(&zerofs_host(), &written(&[unit_path(MOUNT_UNIT)]));
        assert_eq!(
            plan,
            [(ZEROFS_UNIT, false), (MOUNT_UNIT, true), (DAEMON_UNIT, true)]
        );
    }

    #[test]
    fn what_went_wrong_is_read_from_every_unit_the_host_has_not_only_the_one_that_failed() {
        let report = Report {
            units: vec![
                (ZEROFS_UNIT, Outcome::Failed),
                (MOUNT_UNIT, Outcome::Failed),
                (DAEMON_UNIT, Outcome::Started),
            ],
        };
        assert!(!report.all_up());
        assert_eq!(
            report.journal(),
            "journalctl -u nibrunner-zerofs.service -u nibrunner-zerofs-mount.service -u nibrunnerd.service -n 60 --no-pager"
        );
        assert!(Report {
            units: vec![(DAEMON_UNIT, Outcome::Running)]
        }
        .all_up());
    }

    #[test]
    fn a_secret_still_empty_refuses_the_start_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let environment_file = directory.path().join("host.env");
        std::fs::write(&environment_file, "ZEROFS_ENCRYPTION_PASSWORD=\n").unwrap();
        let mut config = zerofs_host();
        config.artifact_store_url = "s3://nibrunner-artifacts/artifacts".to_string();

        let error = refuse_unstartable(&config, &environment_file).unwrap_err();
        let said = error.to_string();
        assert!(said.contains("ZEROFS_ENCRYPTION_PASSWORD is not set"), "{said}");
        assert!(said.contains("AWS_REGION is not set"), "{said}");
        assert!(said.contains("nothing was started"), "{said}");

        assert!(
            refuse_unstartable(&HostConfig::under(Path::new("/srv/nibrunner")), &environment_file).is_ok()
        );
    }

    // The daemon reads it once, at bind time, and a path that is not there is a failed unit and a
    // journal to go and read — said here instead, before anything is asked of systemd.
    #[test]
    fn tls_material_the_configuration_names_has_to_be_there() {
        let directory = tempfile::tempdir().unwrap();
        let certificate = directory.path().join("origin.crt");
        std::fs::write(&certificate, "cert").unwrap();
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        config.proxy.http = Some(HttpListener {
            listen_address: "0.0.0.0".parse().unwrap(),
            port: 443,
            tls: Some(TlsMaterial {
                certificate,
                key: directory.path().join("origin.key"),
                client_ca: None,
            }),
        });

        let error = refuse_unstartable(&config, &directory.path().join("host.env")).unwrap_err();
        let said = error.to_string();
        assert!(said.contains("[proxy.http.tls] key"), "{said}");
        assert!(!said.contains("[proxy.http.tls] certificate"), "{said}");
    }
}
