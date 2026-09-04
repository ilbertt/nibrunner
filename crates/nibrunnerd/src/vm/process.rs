//! Where the Firecracker this daemon carries comes from, and how one is started so that killing
//! this daemon never stops a tenant.
//!
//! Spawned into a session of its own with a process group of its own, so nothing that reaches
//! this daemon's group reaches a guest. While the daemon lives the VMM is still its child, which
//! is what lets an exit code be read; once the daemon is gone the VMM is init's, and the next
//! daemon adopts it from the pidfile rather than from a parent it no longer has.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use protocol::AppId;
use serde::{Deserialize, Serialize};

use crate::json_store::{make_directory, read_json, write_json};
use crate::vm::status::VmStatus;

const FIRECRACKER: &[u8] = include_bytes!(env!("NIBRUNNER_FIRECRACKER_PATH"));
pub const FIRECRACKER_VERSION: &str = env!("NIBRUNNER_FIRECRACKER_VERSION");

const RUNTIME_DIR_MODE: u32 = 0o700;
const EXECUTABLE_MODE: u32 = 0o755;

/// Whether this build carries a hypervisor at all. A build that does not is still worth having —
/// every test but the ones that boot runs on it — and saying so at startup is better than
/// failing at the first deploy.
pub fn carries_firecracker() -> bool {
    // Asked of the build rather than of the bytes: the build script is what knows whether it
    // found a hypervisor to embed, and reading the length back would be inferring that.
    option_env!("NIBRUNNER_FIRECRACKER_EMBEDDED").is_some()
}

/// Extracted to a versioned directory, so two daemons of different versions on one host cannot
/// overwrite each other's copy while a guest is running from it.
pub fn extract_firecracker(directory: &Path) -> std::io::Result<PathBuf> {
    let versioned = directory.join(FIRECRACKER_VERSION);
    let binary = versioned.join("firecracker");
    if !carries_firecracker() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "this build carries no hypervisor, so it can boot nothing",
        ));
    }
    if std::fs::metadata(&binary).is_ok_and(|info| info.len() == FIRECRACKER.len() as u64) {
        return Ok(binary);
    }
    make_directory(&versioned, RUNTIME_DIR_MODE)?;
    // Through a sibling and a rename: a daemon starting while another is extracting must not exec
    // a file that is half written.
    let staged = versioned.join(format!("firecracker.{}.tmp", std::process::id()));
    std::fs::write(&staged, FIRECRACKER)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(EXECUTABLE_MODE))?;
    }
    std::fs::rename(&staged, &binary)?;
    Ok(binary)
}

/// What one running microVM leaves behind for whoever comes next.
///
/// The boot id is what tells a pid that is still alive from one the kernel handed to something
/// else after a reboot: pids are reused, and a host that came back with a different process under
/// the same number would otherwise be a daemon reporting a tenant that is not there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmRecord {
    pub app_id: AppId,
    pub pid: i32,
    pub host_boot_id: String,
    pub started_at_ms: i64,
    /// Written by whoever reaped it. Absent while it is running, and absent forever for one this
    /// daemon did not outlive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the last thing anybody asked of it was to stop, which is what tells an exit that
    /// was wanted from one that was not.
    #[serde(default)]
    pub stop_requested: bool,
}

/// systemd's own, so it changes exactly when the devices and the taps are made again.
const HOST_BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// Read rather than remembered, so a record is compared against the host as it is now. Failing
/// where the file is absent is the point: an unknown boot id that compared equal to another
/// unknown one would let a snapshot survive the reboot it exists to be invalidated by.
pub fn read_host_boot_id() -> std::io::Result<String> {
    std::fs::read_to_string(HOST_BOOT_ID_PATH).map(|value| value.trim().to_string())
}

/// What a host with no boot id of its own is given: this daemon's own start, which changes
/// whenever it restarts. Stricter than the kernel's — it invalidates snapshots a reboot would
/// have left loadable — and it only applies off Linux, where nothing boots anyway.
pub fn host_boot_id_or_session() -> String {
    read_host_boot_id().unwrap_or_else(|_| format!("session-{}", std::process::id()))
}

pub struct VmProcesses {
    runtime_dir: PathBuf,
    boot_id: String,
}

impl VmProcesses {
    pub fn new(runtime_dir: PathBuf) -> Self {
        Self { runtime_dir, boot_id: host_boot_id_or_session() }
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Under a runtime directory that outlives this daemon, which is what leaves a microVM
    /// started by an earlier one still reachable here.
    pub fn api_socket(&self, app_id: &AppId) -> PathBuf {
        self.runtime_dir.join(format!("vm-{app_id}.sock"))
    }

    pub fn record_path(&self, app_id: &AppId) -> PathBuf {
        self.runtime_dir.join(format!("vm-{app_id}.json"))
    }

    /// Where Firecracker's own output goes. Truncated on every start, so the whole of it belongs
    /// to the run in progress — which is what the guest's verdict is read out of, and a stronger
    /// bound than the journal's `--since` because there is nothing older in it to read.
    pub fn console_path(&self, app_id: &AppId) -> PathBuf {
        self.runtime_dir.join(format!("vm-{app_id}.console"))
    }

    pub fn read_record(&self, app_id: &AppId) -> Option<VmRecord> {
        read_json(&self.record_path(app_id)).ok().flatten()
    }

    pub fn write_record(&self, record: &VmRecord) -> std::io::Result<()> {
        make_directory(&self.runtime_dir, RUNTIME_DIR_MODE)?;
        write_json(&self.record_path(&record.app_id), record)
            .map_err(|error| std::io::Error::other(error.message()))
    }

    pub fn forget(&self, app_id: &AppId) {
        let _ = std::fs::remove_file(self.record_path(app_id));
        let _ = std::fs::remove_file(self.api_socket(app_id));
        let _ = std::fs::remove_file(self.console_path(app_id));
    }

    /// Every app this host holds a microVM record for, whether or not this daemon started it.
    pub fn adopted_app_ids(&self) -> Vec<AppId> {
        let Ok(entries) = std::fs::read_dir(&self.runtime_dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let stem = name.strip_prefix("vm-")?.strip_suffix(".json")?;
                AppId::parse(stem).ok()
            })
            .collect()
    }

    /// What can be seen of one microVM without asking the guest anything.
    pub fn status(&self, app_id: &AppId) -> VmStatus {
        let Some(record) = self.read_record(app_id) else {
            return VmStatus::default();
        };
        let started_this_boot = record.host_boot_id == self.boot_id;
        let active = started_this_boot && record.exit_code.is_none() && is_alive(record.pid);
        VmStatus {
            loaded: true,
            active,
            // A non-zero exit nobody asked for is what `failed` means. An exit that was asked for
            // is a stop, and one this daemon never saw is a microVM that outlived it.
            failed: !active && !record.stop_requested && record.exit_code.is_some_and(|code| code != 0),
            started_this_boot,
            exit_code: record.exit_code,
        }
    }

    /// Starts a Firecracker with nothing configured, which is the only kind that will take a
    /// snapshot load. What it boots is decided by whether it is given a config file, and that is
    /// decided here rather than after the fact — `PUT /snapshot/load` is refused by a Firecracker
    /// that has been given any resource but a logger.
    pub async fn spawn(
        &self,
        app_id: &AppId,
        binary: &Path,
        working_dir: &Path,
        config_file: Option<&Path>,
    ) -> std::io::Result<VmRecord> {
        make_directory(&self.runtime_dir, RUNTIME_DIR_MODE)?;
        // Firecracker refuses to bind a socket path that already exists and does not clean its
        // own up on exit, so without this the second boot of any app dies with FailedToBindSocket
        // and never runs again.
        let api_socket = self.api_socket(app_id);
        let _ = std::fs::remove_file(&api_socket);
        let _ = std::fs::remove_file(working_dir.join(guest_contract::vsock::GUEST_VSOCK_FILENAME));

        let console = std::fs::File::create(self.console_path(app_id))?;
        let mut command = tokio::process::Command::new(binary);
        command
            .arg("--api-sock")
            .arg(&api_socket)
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(console.try_clone()?))
            .stderr(Stdio::from(console))
            // Not killed when this handle is dropped: a daemon shutting down must leave every
            // tenant running.
            .kill_on_drop(false);
        if let Some(config_file) = config_file {
            command.arg("--config-file").arg(config_file);
        }
        #[cfg(unix)]
        {
            // Safety: `setsid` is async-signal-safe and is the whole of what runs between fork
            // and exec. Its own session and group is what keeps a signal sent to this daemon's
            // group away from every guest.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn()?;
        let pid = child.id().map(|pid| pid as i32).unwrap_or(-1);
        let record = VmRecord {
            app_id: app_id.clone(),
            pid,
            host_boot_id: self.boot_id.clone(),
            started_at_ms: crate::clock::now_ms(),
            exit_code: None,
            stop_requested: false,
        };
        self.write_record(&record)?;

        // Reaped for as long as this daemon lives, which is what makes an exit code readable at
        // all. A daemon that goes first leaves the VMM to init and the next one reads liveness
        // from the pid instead.
        let processes = Self { runtime_dir: self.runtime_dir.clone(), boot_id: self.boot_id.clone() };
        let app_id = app_id.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|status| status.code()).unwrap_or(-1);
            if let Some(mut record) = processes.read_record(&app_id) {
                if record.pid == pid {
                    record.exit_code = Some(code);
                    let _ = processes.write_record(&record);
                }
            }
        });
        Ok(record)
    }

    /// SIGTERM, then SIGKILL once the guest has had longer than its own shutdown grace to go.
    /// Longer, because a tenant that takes its time shutting down would otherwise look the same
    /// as one that hung.
    pub async fn stop(&self, app_id: &AppId) {
        let Some(mut record) = self.read_record(app_id) else {
            return;
        };
        record.stop_requested = true;
        let _ = self.write_record(&record);
        if !is_alive(record.pid) {
            return;
        }
        signal(record.pid, libc::SIGTERM);
        let deadline = std::time::Duration::from_millis(guest_contract::control::GUEST_SHUTDOWN_GRACE_MS + 5_000);
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            if !is_alive(record.pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        signal(record.pid, libc::SIGKILL);
    }
}

/// Signal 0 asks the kernel whether it could deliver one, which is the whole question.
fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Safety: `kill` with signal 0 delivers nothing and only reports whether it could.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn signal(pid: i32, signal: libc::c_int) {
    if pid > 0 {
        // Safety: the pid is one this daemon recorded having spawned.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::app_id;

    fn processes(directory: &Path) -> VmProcesses {
        VmProcesses { runtime_dir: directory.to_path_buf(), boot_id: "boot-1".into() }
    }

    #[test]
    fn a_host_with_no_record_holds_no_microvm() {
        let directory = tempfile::tempdir().unwrap();
        let processes = processes(directory.path());
        assert_eq!(processes.status(&app_id()), VmStatus::default());
        assert!(processes.adopted_app_ids().is_empty());
    }

    /// The shape a reboot produces: the record survives on disk, and the boot id is what says
    /// nothing has run since the host came up. Reading that as an exit leaves the host serving
    /// nothing until somebody logs in.
    #[test]
    fn a_record_from_before_the_host_rebooted_has_not_run_this_boot() {
        let directory = tempfile::tempdir().unwrap();
        let processes = processes(directory.path());
        processes
            .write_record(&VmRecord {
                app_id: app_id(),
                pid: std::process::id() as i32,
                host_boot_id: "an-earlier-boot".into(),
                started_at_ms: 0,
                exit_code: None,
                stop_requested: false,
            })
            .unwrap();
        let status = processes.status(&app_id());
        assert!(status.loaded);
        assert!(!status.active);
        assert!(!status.started_this_boot);
        assert!(!status.failed);
    }

    #[test]
    fn a_process_this_daemon_can_still_signal_is_running() {
        let directory = tempfile::tempdir().unwrap();
        let processes = processes(directory.path());
        processes
            .write_record(&VmRecord {
                app_id: app_id(),
                pid: std::process::id() as i32,
                host_boot_id: "boot-1".into(),
                started_at_ms: 0,
                exit_code: None,
                stop_requested: false,
            })
            .unwrap();
        let status = processes.status(&app_id());
        assert!(status.active);
        assert!(status.started_this_boot);
        assert_eq!(processes.adopted_app_ids(), vec![app_id()]);
    }

    #[test]
    fn an_exit_nobody_asked_for_is_a_failure_and_one_that_was_asked_for_is_a_stop() {
        let directory = tempfile::tempdir().unwrap();
        let processes = processes(directory.path());
        let record = VmRecord {
            app_id: app_id(),
            pid: 1,
            host_boot_id: "boot-1".into(),
            started_at_ms: 0,
            exit_code: Some(1),
            stop_requested: false,
        };
        processes.write_record(&record).unwrap();
        let crashed = processes.status(&app_id());
        assert!(crashed.failed);
        assert_eq!(crashed.exit_code, Some(1));

        processes.write_record(&VmRecord { stop_requested: true, ..record.clone() }).unwrap();
        assert!(!processes.status(&app_id()).failed);
        // A guest that powered itself off deliberately leaves Firecracker exiting 0, so an exit
        // code of zero is never on its own a failure.
        processes.write_record(&VmRecord { exit_code: Some(0), ..record }).unwrap();
        assert!(!processes.status(&app_id()).failed);
    }

    #[test]
    fn forgetting_a_microvm_takes_its_record_socket_and_console_with_it() {
        let directory = tempfile::tempdir().unwrap();
        let processes = processes(directory.path());
        processes
            .write_record(&VmRecord {
                app_id: app_id(),
                pid: 1,
                host_boot_id: "boot-1".into(),
                started_at_ms: 0,
                exit_code: Some(0),
                stop_requested: true,
            })
            .unwrap();
        std::fs::write(processes.console_path(&app_id()), b"[nibrun] gone\n").unwrap();
        processes.forget(&app_id());
        assert!(processes.read_record(&app_id()).is_none());
        assert!(!processes.console_path(&app_id()).exists());
    }
}

#[cfg(test)]
mod embedding {
    use super::*;

    /// A build that carries a hypervisor extracts it once and reuses it; one that does not says
    /// so rather than failing at the first deploy.
    #[test]
    fn the_embedded_hypervisor_is_extracted_to_a_versioned_directory() {
        let directory = tempfile::tempdir().unwrap();
        if !carries_firecracker() {
            assert!(extract_firecracker(directory.path()).is_err());
            return;
        }
        let binary = extract_firecracker(directory.path()).unwrap();
        assert!(binary.starts_with(directory.path().join(FIRECRACKER_VERSION)));
        let size = std::fs::metadata(&binary).unwrap().len();
        assert!(size > 1_000_000, "a hypervisor is more than a megabyte");
        // A second extraction finds what the first left and does not write again.
        let before = std::fs::metadata(&binary).unwrap().modified().unwrap();
        assert_eq!(extract_firecracker(directory.path()).unwrap(), binary);
        assert_eq!(std::fs::metadata(&binary).unwrap().modified().unwrap(), before);
    }
}
