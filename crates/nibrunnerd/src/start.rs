//! `nibrunnerd start`: the host `install` laid out, asked of systemd. Everything before this point
//! writes files; this is the one place that asks for them to be run, and it asks only after
//! checking what a start would otherwise find out too late and say from too far away — a secret
//! still empty in `host.env`, a certificate the configuration names that is not there or that
//! holds fewer certificates than it marks. With those checked, a start that fails is one the
//! configuration got wrong.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::adapters::proxy::pem::read_certificates;
use crate::config::HostConfig;
use crate::install::render::{DAEMON_UNIT, MOUNT_UNIT, ZEROFS_UNIT};
use crate::install::{secrets, SYSTEMD_DIR};

mod inputs;

use inputs::Inputs;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("this host cannot be started:\n{0}\nnothing was started")]
    Unstartable(String),
    #[error("{0}")]
    Systemd(String),
    #[error(transparent)]
    Inputs(#[from] crate::json_store::StoreError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Started,
    Restarted,
    Reloaded,
    /// Left as it was, because nothing it reads changed.
    Running,
    Failed,
}

impl Outcome {
    pub fn said(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Restarted => "restarted",
            Self::Reloaded => "reloaded",
            Self::Running => "running",
            Self::Failed => "failed",
        }
    }
}

pub struct Report {
    pub units: Vec<(&'static str, Outcome)>,
    /// `host.env` was edited after ZeroFS last started, and ZeroFS was left running.
    pub kept_credentials: Option<KeptCredentials>,
}

/// ZeroFS expands the object-store key and the encryption password out of its environment once,
/// on the way up, and a start that leaves it running — every start that changed nothing it reads —
/// leaves it with the ones it started with. Not a reason to restart it from here: that drops
/// every NBD device under every guest, and only the operator knows when no guest needs its disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptCredentials {
    pub environment_file: PathBuf,
}

impl KeptCredentials {
    pub fn said(&self) -> String {
        format!(
            "{} changed after {ZEROFS_UNIT} last started, and ZeroFS reads it only on the way up: it keeps the credentials it started with until `systemctl restart nibrunner-zerofs` is run. Run that at a moment no guest needs its disk, because the restart drops every device under every guest.",
            self.environment_file.display()
        )
    }
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

pub fn run(config: &HostConfig, environment_file: &Path, restart_zerofs: bool) -> Result<Report, StartError> {
    refuse_unstartable(config, environment_file)?;
    let binary = std::env::current_exe()
        .map_err(|error| StartError::Systemd(format!("the daemon executable could not be found: {error}")))?;
    let plan = plan(config, Path::new(SYSTEMD_DIR), &binary)?;
    let units = activate(config, &plan, restart_zerofs, &Systemd)?;
    let kept_credentials = zerofs_kept_its_credentials(&units, || {
        let edited = std::fs::metadata(environment_file).ok()?.modified().ok()?;
        Some((edited, active_since(ZEROFS_UNIT)?))
    })
    .then(|| KeptCredentials {
        environment_file: environment_file.to_path_buf(),
    });
    Ok(Report {
        units,
        kept_credentials,
    })
}

fn activate(
    config: &HostConfig,
    plan: &[Service],
    restart_zerofs: bool,
    systemd: &impl ServiceControl,
) -> Result<Vec<(&'static str, Outcome)>, StartError> {
    if !restart_zerofs
        && plan.iter().any(|service| {
            service.unit == ZEROFS_UNIT && service.inputs.pending && systemd.is_active(ZEROFS_UNIT)
        })
    {
        return Err(zerofs_restart_requires_permission());
    }
    must(systemd, &["daemon-reload"])?;
    let mut enable = vec!["enable"];
    enable.extend(plan.iter().map(|service| service.unit));
    must(systemd, &enable)?;

    let mut units = Vec::new();
    let mut restart_mount = false;
    for service in plan {
        let unit = service.unit;
        let was_active = systemd.is_active(unit);
        let pending = service.inputs.pending || (unit == MOUNT_UNIT && restart_mount);
        if unit == ZEROFS_UNIT && pending && was_active && !restart_zerofs {
            return Err(zerofs_restart_requires_permission());
        }
        if unit != DAEMON_UNIT && was_active && !pending {
            units.push((unit, Outcome::Running));
            continue;
        }
        if unit == DAEMON_UNIT && was_active && !service.inputs.pending {
            match systemd.reload(config)? {
                Some(crate::reload::Applied::Unchanged) => {
                    units.push((unit, Outcome::Running));
                    continue;
                }
                Some(crate::reload::Applied::Grown) => {
                    units.push((unit, Outcome::Reloaded));
                    continue;
                }
                None => {}
            }
        }
        let restart = pending || unit == DAEMON_UNIT;
        // A failed or interrupted activation may already have replaced the process, even if
        // the operator subsequently restores the previous files.
        service.inputs.invalidate()?;
        if unit == ZEROFS_UNIT {
            restart_mount = true;
            for mount in plan.iter().filter(|service| service.unit == MOUNT_UNIT) {
                mount.inputs.invalidate()?;
            }
        }
        let verb = if restart { "restart" } else { "start" };
        let outcome = match systemd.command(&[verb, unit])? {
            true if !systemd.is_active(unit) => Outcome::Failed,
            true if !was_active => Outcome::Started,
            true if restart => Outcome::Restarted,
            true => Outcome::Running,
            false => Outcome::Failed,
        };
        units.push((unit, outcome));
        if outcome == Outcome::Failed {
            break;
        }
    }
    // `Type=exec` calls a unit started the moment it execs, and one that exits right after is
    // read as started by the check above — and as auto-restarting by this one, once the units
    // after it have taken their time coming up.
    for (unit, outcome) in &mut units {
        if *outcome != Outcome::Failed && !systemd.is_active(unit) {
            *outcome = Outcome::Failed;
        }
    }
    for (service, (_, outcome)) in plan.iter().zip(&units) {
        if *outcome == Outcome::Failed {
            service.inputs.invalidate()?;
        } else {
            service.inputs.applied()?;
        }
    }
    Ok(units)
}

fn zerofs_restart_requires_permission() -> StartError {
    StartError::Unstartable(format!(
        "  {ZEROFS_UNIT} has unapplied or unrecorded service inputs. Restarting ZeroFS disconnects every guest disk. Stop the guests that use it, then run `nibrunnerd start --restart-zerofs`"
    ))
}

/// Whether ZeroFS was left running on an environment file edited after it came up. Only a unit
/// left as it was can be: one this run started or restarted read the file on its way up. The
/// clocks are asked for only then, so a host without ZeroFS never asks systemd about it.
fn zerofs_kept_its_credentials(
    units: &[(&'static str, Outcome)],
    clocks: impl FnOnce() -> Option<(SystemTime, SystemTime)>,
) -> bool {
    let left_running = units
        .iter()
        .any(|(unit, outcome)| *unit == ZEROFS_UNIT && *outcome == Outcome::Running);
    if !left_running {
        return false;
    }
    clocks().is_some_and(|(edited, active_since)| edited > active_since)
}

/// When the unit last became active, on the wall clock. systemd keeps it on the monotonic clock,
/// and a file's mtime is on the other one, so the two are brought together through now on both.
fn active_since(unit: &str) -> Option<SystemTime> {
    let output = Command::new("systemctl")
        .args(["show", "-p", "ActiveEnterTimestampMonotonic", "--value", unit])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let since_boot = monotonic_since_boot(&String::from_utf8_lossy(&output.stdout))?;
    let now = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).ok()?;
    let now_since_boot = Duration::new(now.tv_sec().try_into().ok()?, now.tv_nsec().try_into().ok()?);
    wall_clock_of(since_boot, now_since_boot, SystemTime::now())
}

/// systemd prints microseconds since boot, and `0` for a unit that has never been active.
fn monotonic_since_boot(shown: &str) -> Option<Duration> {
    let microseconds: u64 = shown.trim().parse().ok()?;
    (microseconds > 0).then(|| Duration::from_micros(microseconds))
}

fn wall_clock_of(since_boot: Duration, now_since_boot: Duration, now: SystemTime) -> Option<SystemTime> {
    now.checked_sub(now_since_boot.checked_sub(since_boot)?)
}

struct Service {
    unit: &'static str,
    inputs: Inputs,
}

fn plan(config: &HostConfig, systemd_dir: &Path, binary: &Path) -> Result<Vec<Service>, StartError> {
    let mut plan = Vec::new();
    let service = |unit, paths| {
        Ok::<_, StartError>(Service {
            unit,
            inputs: Inputs::read(paths, inputs::receipt(&config.state_dir, unit))?,
        })
    };
    if let Some(settings) = config.volumes.zerofs() {
        let zerofs = vec![
            settings.binary.clone(),
            settings.config_file.clone(),
            systemd_dir.join(ZEROFS_UNIT),
        ];
        plan.push(service(ZEROFS_UNIT, zerofs.clone())?);
        let mut mount = zerofs;
        mount.push(systemd_dir.join(MOUNT_UNIT));
        plan.push(service(MOUNT_UNIT, mount)?);
    }
    let mut daemon = vec![
        binary.to_path_buf(),
        systemd_dir.join(DAEMON_UNIT),
        systemd_dir.join(crate::install::render::DAEMON_DROP_IN),
    ];
    if let Some(settings) = config.volumes.zerofs() {
        daemon.push(settings.checkpoint_config_file.clone());
    }
    plan.push(service(DAEMON_UNIT, daemon)?);
    Ok(plan)
}

fn refuse_unstartable(config: &HostConfig, environment_file: &Path) -> Result<(), StartError> {
    let mut reasons: Vec<String> = secrets::missing(config, environment_file)
        .into_iter()
        .map(|name| format!("  {name} is not set in {}", environment_file.display()))
        .collect();
    for (what, path, holds) in tls_material(config) {
        if !path.is_file() {
            reasons.push(format!(
                "  [proxy.http.tls] {what} {} is not there",
                path.display()
            ));
        } else if holds == Holds::Certificates {
            if let Err(error) = read_certificates(path) {
                reasons.push(format!("  [proxy.http.tls] {what} {error}"));
            }
        }
    }
    for (section, address) in listeners(config) {
        if !this_host_has(address) {
            reasons.push(format!(
                "  [{section}] listen_address {address} is not an address this host has"
            ));
        }
    }
    if reasons.is_empty() {
        return Ok(());
    }
    Err(StartError::Unstartable(reasons.join("\n")))
}

fn listeners(config: &HostConfig) -> Vec<(&'static str, IpAddr)> {
    let mut listeners = Vec::new();
    if let Some(http) = &config.proxy.http {
        listeners.push(("proxy.http", http.listen_address));
    }
    if let Some(raw) = &config.proxy.raw {
        listeners.push(("proxy.raw", raw.listen_address));
    }
    if let Some(metrics) = &config.metrics {
        listeners.push(("metrics", metrics.listen_address));
    }
    listeners
}

/// Asked of the kernel the way the daemon will ask it, on a port it hands out: binding an address
/// this host does not have fails the same way whichever port is named, and this way needs no
/// privilege and takes nothing the daemon will want.
fn this_host_has(address: IpAddr) -> bool {
    address.is_unspecified() || UdpSocket::bind(SocketAddr::new(address, 0)).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holds {
    /// Read here the way the daemon reads it, because a file that is there can still be short.
    Certificates,
    Key,
}

fn tls_material(config: &HostConfig) -> Vec<(&'static str, &Path, Holds)> {
    let Some(tls) = config.proxy.http.as_ref().and_then(|http| http.tls.as_ref()) else {
        return Vec::new();
    };
    let mut material = vec![
        ("certificate", tls.certificate.as_path(), Holds::Certificates),
        ("key", tls.key.as_path(), Holds::Key),
    ];
    if let Some(client_ca) = &tls.client_ca {
        material.push(("client_ca.certificate", client_ca.as_path(), Holds::Certificates));
    }
    material
}

/// Longer than any unit here can take to start or stop — the mount waits up to twenty seconds,
/// ZeroFS is given two minutes to stop — so reaching it means a job that is not going to finish,
/// which is a thing to be told about rather than waited on.
const SYSTEMCTL_DEADLINE: Duration = Duration::from_secs(300);

fn must(systemd: &impl ServiceControl, arguments: &[&str]) -> Result<(), StartError> {
    if systemd.command(arguments)? {
        return Ok(());
    }
    Err(StartError::Systemd(format!(
        "systemctl {} failed",
        arguments.join(" ")
    )))
}

trait ServiceControl {
    fn command(&self, arguments: &[&str]) -> Result<bool, StartError>;
    fn is_active(&self, unit: &str) -> bool;
    fn reload(&self, config: &HostConfig) -> Result<Option<crate::reload::Applied>, StartError>;
}

struct Systemd;

impl ServiceControl for Systemd {
    fn command(&self, arguments: &[&str]) -> Result<bool, StartError> {
        systemctl(arguments)
    }

    fn is_active(&self, unit: &str) -> bool {
        is_active(unit)
    }

    fn reload(&self, config: &HostConfig) -> Result<Option<crate::reload::Applied>, StartError> {
        crate::reload::request(config).map_err(|error| {
            StartError::Systemd(format!(
                "the running host could not confirm its configuration: {error}"
            ))
        })
    }
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

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;

    use crate::install::{render, write_generated, Laid};

    #[derive(Default)]
    struct FakeSystemd {
        active: RefCell<BTreeSet<String>>,
        commands: RefCell<Vec<Vec<String>>>,
        fail: Cell<Option<&'static str>>,
        interrupt: Cell<Option<&'static str>>,
        exit_after: Cell<Option<&'static str>>,
    }

    impl ServiceControl for FakeSystemd {
        fn command(&self, arguments: &[&str]) -> Result<bool, StartError> {
            self.commands
                .borrow_mut()
                .push(arguments.iter().map(|arg| (*arg).to_string()).collect());
            if matches!(arguments[0], "start" | "restart") {
                let unit = arguments[1];
                self.active.borrow_mut().insert(unit.to_string());
                if self.exit_after.get() == Some(unit) {
                    std::process::exit(77);
                }
                if self.interrupt.get() == Some(unit) {
                    return Err(StartError::Systemd("activation interrupted".to_string()));
                }
                if self.fail.get() == Some(unit) {
                    return Ok(false);
                }
            }
            Ok(true)
        }

        fn is_active(&self, unit: &str) -> bool {
            self.active.borrow().contains(unit)
        }

        fn reload(&self, _: &HostConfig) -> Result<Option<crate::reload::Applied>, StartError> {
            Ok(Some(crate::reload::Applied::Unchanged))
        }
    }

    struct HostFiles {
        directory: tempfile::TempDir,
        config: HostConfig,
    }

    impl HostFiles {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let mut config = HostConfig::under(root);
            config.volumes = VolumeBackend::Zerofs(Box::new(zerofs_settings(|settings| {
                settings.binary = root.join("zerofs");
                settings.config_file = root.join("zerofs.toml");
                settings.checkpoint_config_file = root.join("checkpoint.toml");
            })));
            std::fs::write(root.join("zerofs"), "zerofs binary").unwrap();
            std::fs::write(root.join("nibrunnerd"), "daemon binary").unwrap();
            Self { directory, config }
        }

        fn install(&self) -> Laid {
            let root = self.directory.path();
            let file = root.join("config.toml");
            std::fs::write(&file, self.config.to_toml()).unwrap();
            let environment = crate::install::environment_file(&file);
            let binary = root.join("nibrunnerd");
            let mut files = vec![
                (DAEMON_UNIT.to_string(), render::daemon_unit(&file, &binary)),
                (
                    render::DAEMON_DROP_IN.to_string(),
                    render::daemon_drop_in(&self.config, &file, &environment),
                ),
            ];
            let mut laid = Laid::default();
            if let Some(settings) = self.config.volumes.zerofs() {
                write_generated(
                    &settings.config_file,
                    &render::zerofs_config(settings, &file, &environment),
                    false,
                    &mut laid,
                )
                .unwrap();
                write_generated(
                    &settings.checkpoint_config_file,
                    &render::checkpoint_config(settings, &file),
                    false,
                    &mut laid,
                )
                .unwrap();
                files.extend([
                    (
                        ZEROFS_UNIT.to_string(),
                        render::zerofs_unit(settings, &file, &environment),
                    ),
                    (MOUNT_UNIT.to_string(), render::mount_unit(settings, &file)),
                ]);
            }
            for (unit, body) in files {
                write_generated(&root.join("systemd").join(unit), &body, false, &mut laid).unwrap();
            }
            laid
        }

        fn plan(&self) -> Vec<Service> {
            let root = self.directory.path();
            plan(&self.config, &root.join("systemd"), &root.join("nibrunnerd")).unwrap()
        }

        fn start(
            &self,
            systemd: &FakeSystemd,
            restart_zerofs: bool,
        ) -> Result<Vec<(&'static str, Outcome)>, StartError> {
            activate(&self.config, &self.plan(), restart_zerofs, systemd)
        }

        fn pending(&self) -> Vec<bool> {
            self.plan().iter().map(|service| service.inputs.pending).collect()
        }

        fn invocation(&self) -> Command {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "start::tests::a_start_invocation_reads_only_persisted_inputs",
                    "--nocapture",
                ])
                .env("NIBRUNNER_START_TEST_ROOT", self.directory.path());
            command
        }

        fn invoke_start(&self, expected: &str) {
            let output = self
                .invocation()
                .env("NIBRUNNER_START_TEST_OUTCOME", expected)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn change_cache(&mut self) {
            let VolumeBackend::Zerofs(settings) = &mut self.config.volumes else {
                panic!("ZeroFS host")
            };
            settings.cache_disk_mib += 1024;
        }
    }

    #[test]
    fn separate_install_and_start_processes_apply_pending_inputs_once() {
        let mut host = HostFiles::new();
        host.install();
        host.invoke_start("restarted");
        host.invoke_start("running");
        host.change_cache();
        host.install();
        assert!(host.install().done.is_empty());
        host.invoke_start("restarted");
        host.invoke_start("running");
    }

    #[test]
    fn a_start_invocation_reads_only_persisted_inputs() {
        let Some(root) = std::env::var_os("NIBRUNNER_START_TEST_ROOT").map(PathBuf::from) else {
            return;
        };
        let config = HostConfig::from_file(&root.join("config.toml")).unwrap();
        let plan = plan(&config, &root.join("systemd"), &root.join("nibrunnerd")).unwrap();
        let systemd = FakeSystemd::default();
        if std::env::var_os("NIBRUNNER_START_TEST_INTERRUPT").is_some() {
            systemd.exit_after.set(Some(ZEROFS_UNIT));
        }
        systemd
            .active
            .borrow_mut()
            .extend([ZEROFS_UNIT, MOUNT_UNIT, DAEMON_UNIT].map(str::to_string));
        let outcomes = activate(&config, &plan, true, &systemd).unwrap();
        assert_eq!(
            outcomes[0].1.said(),
            std::env::var("NIBRUNNER_START_TEST_OUTCOME").unwrap()
        );
    }

    #[test]
    fn a_process_interrupted_after_restarting_zerofs_leaves_work_for_the_next_process() {
        let mut host = HostFiles::new();
        host.install();
        host.invoke_start("restarted");
        host.change_cache();
        host.install();
        let status = host
            .invocation()
            .env("NIBRUNNER_START_TEST_INTERRUPT", "1")
            .output()
            .unwrap()
            .status;
        assert_eq!(status.code(), Some(77));
        assert!(host.install().done.is_empty());
        assert_eq!(host.pending(), [true, true, false]);
        host.invoke_start("restarted");
        host.invoke_start("running");
    }

    #[test]
    fn an_active_host_without_receipts_is_not_assumed_to_have_applied_its_files() {
        let host = HostFiles::new();
        host.install();
        let systemd = FakeSystemd::default();
        systemd.active.borrow_mut().insert(ZEROFS_UNIT.to_string());
        assert!(host.start(&systemd, false).is_err());
        assert!(systemd.commands.borrow().is_empty());
        assert_eq!(host.pending(), [true, true, true]);
    }

    #[test]
    fn starting_a_stopped_zerofs_also_refreshes_an_active_mount() {
        let host = HostFiles::new();
        host.install();
        let systemd = FakeSystemd::default();
        host.start(&systemd, false).unwrap();
        systemd.active.borrow_mut().remove(ZEROFS_UNIT);
        let outcomes = host.start(&systemd, false).unwrap();
        assert_eq!(outcomes[0].1, Outcome::Started);
        assert_eq!(outcomes[1].1, Outcome::Restarted);
    }

    #[test]
    fn unapplied_zerofs_inputs_require_explicit_restart_permission_on_an_active_host() {
        let mut host = HostFiles::new();
        host.install();
        let systemd = FakeSystemd::default();
        host.start(&systemd, false).unwrap();
        host.change_cache();
        host.install();
        systemd.commands.borrow_mut().clear();
        let error = host.start(&systemd, false).unwrap_err();
        assert!(error.to_string().contains("--restart-zerofs"), "{error}");
        assert!(systemd.commands.borrow().is_empty());
        assert_eq!(host.pending(), [true, true, false]);
        host.start(&systemd, true).unwrap();
        assert_eq!(host.pending(), [false, false, false]);
    }

    #[test]
    fn an_interrupted_activation_does_not_acknowledge_the_new_inputs() {
        let mut host = HostFiles::new();
        host.install();
        let systemd = FakeSystemd::default();
        host.start(&systemd, false).unwrap();
        host.change_cache();
        host.install();
        systemd.interrupt.set(Some(MOUNT_UNIT));
        assert!(host.start(&systemd, true).is_err());
        assert_eq!(host.pending(), [true, true, false]);
        systemd.interrupt.set(None);
        host.start(&systemd, true).unwrap();
        assert_eq!(host.pending(), [false, false, false]);
    }

    #[test]
    fn a_failed_restart_remains_pending_even_if_the_old_files_are_restored() {
        let mut host = HostFiles::new();
        host.install();
        let old = host.config.clone();
        let systemd = FakeSystemd::default();
        host.start(&systemd, false).unwrap();
        host.change_cache();
        host.install();
        systemd.fail.set(Some(ZEROFS_UNIT));
        let outcomes = host.start(&systemd, true).unwrap();
        assert_eq!(outcomes, [(ZEROFS_UNIT, Outcome::Failed)]);
        assert_eq!(host.pending(), [true, true, false]);
        host.config = old;
        host.install();
        assert_eq!(host.pending(), [true, true, false]);
        systemd.fail.set(None);
        host.start(&systemd, true).unwrap();
        assert_eq!(host.pending(), [false, false, false]);
    }

    #[test]
    fn each_changed_zerofs_input_also_leaves_the_mount_pending() {
        let host = HostFiles::new();
        host.install();
        let systemd = FakeSystemd::default();
        host.start(&systemd, false).unwrap();
        let settings = host.config.volumes.zerofs().unwrap();
        for path in [
            settings.binary.clone(),
            settings.config_file.clone(),
            host.directory.path().join("systemd").join(ZEROFS_UNIT),
        ] {
            std::fs::write(&path, "changed").unwrap();
            assert_eq!(host.pending(), [true, true, false]);
            host.start(&systemd, true).unwrap();
        }
    }

    #[test]
    fn mount_and_checkpoint_changes_only_restart_their_own_consumers() {
        let host = HostFiles::new();
        host.install();
        let systemd = FakeSystemd::default();
        host.start(&systemd, false).unwrap();
        std::fs::write(host.directory.path().join("systemd").join(MOUNT_UNIT), "changed").unwrap();
        assert_eq!(host.pending(), [false, true, false]);
        let outcomes = host.start(&systemd, false).unwrap();
        assert_eq!(outcomes[0].1, Outcome::Running);
        assert_eq!(outcomes[1].1, Outcome::Restarted);
        std::fs::write(
            &host.config.volumes.zerofs().unwrap().checkpoint_config_file,
            "changed",
        )
        .unwrap();
        assert_eq!(host.pending(), [false, false, true]);
        let outcomes = host.start(&systemd, false).unwrap();
        assert_eq!(outcomes[0].1, Outcome::Running);
        assert_eq!(outcomes[2].1, Outcome::Restarted);
    }

    #[test]
    fn a_local_file_host_can_reload_after_an_unchanged_separate_install() {
        let mut host = HostFiles::new();
        host.config.volumes = VolumeBackend::LocalFile;
        host.install();
        let systemd = FakeSystemd::default();
        assert_eq!(
            host.start(&systemd, false).unwrap(),
            [(DAEMON_UNIT, Outcome::Started)]
        );
        assert!(host.install().done.is_empty());
        assert_eq!(
            host.start(&systemd, false).unwrap(),
            [(DAEMON_UNIT, Outcome::Running)]
        );
        std::fs::write(host.directory.path().join("systemd").join(DAEMON_UNIT), "changed").unwrap();
        assert_eq!(
            host.start(&systemd, false).unwrap(),
            [(DAEMON_UNIT, Outcome::Restarted)]
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
            kept_credentials: None,
        };
        assert!(!report.all_up());
        assert_eq!(
            report.journal(),
            "journalctl -u nibrunner-zerofs.service -u nibrunner-zerofs-mount.service -u nibrunnerd.service -n 60 --no-pager"
        );
        assert!(Report {
            units: vec![(DAEMON_UNIT, Outcome::Running)],
            kept_credentials: None,
        }
        .all_up());
    }

    fn zerofs_left(outcome: Outcome) -> Vec<(&'static str, Outcome)> {
        vec![
            (ZEROFS_UNIT, outcome),
            (MOUNT_UNIT, Outcome::Running),
            (DAEMON_UNIT, Outcome::Restarted),
        ]
    }

    // A rotated key or a corrected password in host.env reaches the daemon, which is always
    // restarted, and never a ZeroFS left running — and restarting ZeroFS from here would drop
    // every device under every guest. So it is said, once, on the start that left it running.
    #[test]
    fn zerofs_left_running_on_an_environment_file_edited_since_it_came_up_is_said() {
        let came_up = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let edited_after = came_up + Duration::from_secs(60);
        let edited_before = came_up - Duration::from_secs(60);

        assert!(zerofs_kept_its_credentials(
            &zerofs_left(Outcome::Running),
            || Some((edited_after, came_up))
        ));
        assert!(!zerofs_kept_its_credentials(
            &zerofs_left(Outcome::Running),
            || Some((edited_before, came_up))
        ));
        assert!(
            !zerofs_kept_its_credentials(&zerofs_left(Outcome::Running), || Some((came_up, came_up))),
            "written in the same instant it came up is what it read"
        );
    }

    #[test]
    fn a_zerofs_this_run_brought_up_read_the_environment_file_as_it_is() {
        let came_up = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let edited_after = came_up + Duration::from_secs(60);
        for outcome in [Outcome::Started, Outcome::Restarted, Outcome::Failed] {
            assert!(
                !zerofs_kept_its_credentials(&zerofs_left(outcome), || Some((edited_after, came_up))),
                "{}",
                outcome.said()
            );
        }
        assert!(!zerofs_kept_its_credentials(
            &[(DAEMON_UNIT, Outcome::Restarted)],
            || panic!("a host without ZeroFS has nothing to ask systemd about")
        ));
        assert!(
            !zerofs_kept_its_credentials(&zerofs_left(Outcome::Running), || None),
            "a clock that cannot be read is no claim either way"
        );
    }

    #[test]
    fn what_systemd_shows_of_the_monotonic_clock_is_brought_onto_the_wall_clock() {
        assert_eq!(
            monotonic_since_boot("1500000\n"),
            Some(Duration::from_micros(1_500_000))
        );
        assert_eq!(monotonic_since_boot("0\n"), None, "never active");
        assert_eq!(monotonic_since_boot(""), None);
        assert_eq!(monotonic_since_boot("Failed to get properties"), None);

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        assert_eq!(
            wall_clock_of(Duration::from_secs(100), Duration::from_secs(400), now),
            Some(now - Duration::from_secs(300))
        );
        assert_eq!(
            wall_clock_of(Duration::from_secs(500), Duration::from_secs(400), now),
            None,
            "active since after now is a clock that cannot be trusted"
        );
    }

    #[test]
    fn what_is_said_names_the_file_the_command_and_when_to_run_it() {
        let said = KeptCredentials {
            environment_file: PathBuf::from("/etc/nibrunner/host.env"),
        }
        .said();
        assert!(
            said.starts_with("/etc/nibrunner/host.env changed after nibrunner-zerofs.service last started"),
            "{said}"
        );
        assert!(said.contains("keeps the credentials it started with"), "{said}");
        assert!(said.contains("`systemctl restart nibrunner-zerofs`"), "{said}");
        assert!(said.contains("no guest needs its disk"), "{said}");
    }

    #[test]
    fn a_secret_still_empty_refuses_the_start_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let environment_file = directory.path().join("host.env");
        std::fs::write(&environment_file, "ZEROFS_ENCRYPTION_PASSWORD=\n").unwrap();
        let mut config = HostFiles::new().config;
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

    // The example configuration names a relay's private address for raw ports, and a host that
    // copied it as it is would bind an address it does not have — a second after `start` had
    // called it up. Said here instead, before anything is asked of systemd.
    #[test]
    fn a_listen_address_this_host_does_not_have_refuses_the_start_by_name() {
        use crate::config::RawPorts;
        let directory = tempfile::tempdir().unwrap();
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        config.proxy.raw = Some(RawPorts {
            // TEST-NET-1: documentation only, so no host has it.
            listen_address: "192.0.2.1".parse().unwrap(),
            max_ports_per_guest: 1,
        });
        config.metrics = Some(crate::config::MetricsConfig {
            port: 9100,
            listen_address: "127.0.0.1".parse().unwrap(),
        });

        let error = refuse_unstartable(&config, &directory.path().join("host.env")).unwrap_err();
        let said = error.to_string();
        assert!(said.contains("[proxy.raw] listen_address 192.0.2.1"), "{said}");
        assert!(!said.contains("[metrics]"), "{said}");

        assert!(this_host_has("0.0.0.0".parse().unwrap()));
        assert!(this_host_has("::".parse().unwrap()));
    }

    const A_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";

    fn serving_tls(directory: &Path, client_ca: Option<PathBuf>) -> HostConfig {
        let certificate = directory.join("origin.crt");
        std::fs::write(&certificate, A_CERTIFICATE).unwrap();
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        config.proxy.http = Some(HttpListener {
            concurrency: None,
            listen_address: "0.0.0.0".parse().unwrap(),
            port: 443,
            tls: Some(TlsMaterial {
                certificate,
                key: directory.join("origin.key"),
                client_ca,
            }),
        });
        config
    }

    // The daemon reads it once, at bind time, and a path that is not there is a failed unit and a
    // journal to go and read — said here instead, before anything is asked of systemd.
    #[test]
    fn tls_material_the_configuration_names_has_to_be_there() {
        let directory = tempfile::tempdir().unwrap();
        let config = serving_tls(directory.path(), None);

        let error = refuse_unstartable(&config, &directory.path().join("host.env")).unwrap_err();
        let said = error.to_string();
        assert!(said.contains("[proxy.http.tls] key"), "{said}");
        assert!(!said.contains("[proxy.http.tls] certificate"), "{said}");
    }

    // Two CA files concatenated where the first has no trailing newline glue an END marker to the
    // next BEGIN, and the daemon would load one certificate of the two without a word and refuse
    // every caller the second one signed. Said here, with the counts, before it is started.
    #[test]
    fn a_trust_pool_short_of_what_it_marks_refuses_the_start_with_the_counts() {
        let directory = tempfile::tempdir().unwrap();
        let pool = directory.path().join("origin-pull-ca.pem");
        std::fs::write(&pool, format!("{A_CERTIFICATE}{A_CERTIFICATE}\n")).unwrap();
        std::fs::write(directory.path().join("origin.key"), "").unwrap();
        let config = serving_tls(directory.path(), Some(pool.clone()));

        let error = refuse_unstartable(&config, &directory.path().join("host.env")).unwrap_err();
        let said = error.to_string();
        assert!(
            said.contains(&format!(
                "  [proxy.http.tls] client_ca.certificate {} marks 2 certificates and 1 could be read",
                pool.display()
            )),
            "{said}"
        );
        assert!(!said.contains("[proxy.http.tls] certificate"), "{said}");
        assert!(!said.contains("[proxy.http.tls] key"), "{said}");
    }
}
