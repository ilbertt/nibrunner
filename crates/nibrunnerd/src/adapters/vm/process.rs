use std::path::{Path, PathBuf};
use std::process::Stdio;

use protocol::AppId;
use serde::{Deserialize, Serialize};

use crate::adapters::vm::status::VmStatus;
use crate::json_store::{make_directory, read_json, write_json};

const FIRECRACKER: &[u8] = include_bytes!(env!("NIBRUNNER_FIRECRACKER_PATH"));
pub const FIRECRACKER_VERSION: &str = env!("NIBRUNNER_FIRECRACKER_VERSION");

const RUNTIME_DIR_MODE: u32 = 0o700;
const EXECUTABLE_MODE: u32 = 0o755;

pub fn carries_firecracker() -> bool {
    option_env!("NIBRUNNER_FIRECRACKER_EMBEDDED").is_some()
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmRecord {
    pub app_id: AppId,
    pub pid: i32,
    pub host_boot_id: String,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stop_requested: bool,
}

const HOST_BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

pub fn read_host_boot_id() -> std::io::Result<String> {
    std::fs::read_to_string(HOST_BOOT_ID_PATH).map(|value| value.trim().to_string())
}

pub fn host_boot_id_or_session() -> String {
    read_host_boot_id().unwrap_or_else(|_| format!("session-{}", std::process::id()))
}

pub struct VmProcesses {
    runtime_dir: PathBuf,
    boot_id: String,
}

impl VmProcesses {
    pub fn new(runtime_dir: PathBuf) -> Self {
        Self {
            runtime_dir,
            boot_id: host_boot_id_or_session(),
        }
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    pub fn api_socket(&self, app_id: &AppId) -> PathBuf {
        self.runtime_dir.join(format!("vm-{app_id}.sock"))
    }

    pub fn record_path(&self, app_id: &AppId) -> PathBuf {
        self.runtime_dir.join(format!("vm-{app_id}.json"))
    }

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

    pub fn status(&self, app_id: &AppId) -> VmStatus {
        let Some(record) = self.read_record(app_id) else {
            return VmStatus::default();
        };
        let started_this_boot = record.host_boot_id == self.boot_id;
        let active = started_this_boot && record.exit_code.is_none() && is_alive(record.pid);
        VmStatus {
            loaded: true,
            active,
            failed: !active && !record.stop_requested && record.exit_code.is_some_and(|code| code != 0),
            started_this_boot,
            exit_code: record.exit_code,
        }
    }

    pub async fn spawn(
        &self,
        app_id: &AppId,
        binary: &Path,
        working_dir: &Path,
        config_file: Option<&Path>,
    ) -> std::io::Result<VmRecord> {
        make_directory(&self.runtime_dir, RUNTIME_DIR_MODE)?;
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
            .kill_on_drop(false);
        if let Some(config_file) = config_file {
            command.arg("--config-file").arg(config_file);
        }
        #[cfg(unix)]
        {
            #[allow(
                unsafe_code,
                reason = "there is no safe way to call setsid between fork and exec"
            )]
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

        let processes = Self {
            runtime_dir: self.runtime_dir.clone(),
            boot_id: self.boot_id.clone(),
        };
        let app_id = app_id.clone();
        tokio::spawn(async move {
            let code = child
                .wait()
                .await
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(-1);
            if let Some(mut record) = processes.read_record(&app_id) {
                if record.pid == pid {
                    record.exit_code = Some(code);
                    let _ = processes.write_record(&record);
                }
            }
        });
        Ok(record)
    }

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
        let deadline =
            std::time::Duration::from_millis(guest_contract::control::GUEST_SHUTDOWN_GRACE_MS + 5_000);
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

fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[allow(
        unsafe_code,
        reason = "asking the kernel whether a pid exists has no safe spelling"
    )]
    unsafe {
        libc::kill(pid, 0) == 0
    }
}

fn signal(pid: i32, signal: libc::c_int) {
    if pid > 0 {
        #[allow(unsafe_code, reason = "signalling a recorded pid has no safe spelling")]
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
        VmProcesses {
            runtime_dir: directory.to_path_buf(),
            boot_id: "boot-1".into(),
        }
    }

    #[test]
    fn a_host_with_no_record_holds_no_microvm() {
        let directory = tempfile::tempdir().unwrap();
        let processes = processes(directory.path());
        assert_eq!(processes.status(&app_id()), VmStatus::default());
        assert!(processes.adopted_app_ids().is_empty());
    }

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

        processes
            .write_record(&VmRecord {
                stop_requested: true,
                ..record.clone()
            })
            .unwrap();
        assert!(!processes.status(&app_id()).failed);
        processes
            .write_record(&VmRecord {
                exit_code: Some(0),
                ..record
            })
            .unwrap();
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
        let before = std::fs::metadata(&binary).unwrap().modified().unwrap();
        assert_eq!(extract_firecracker(directory.path()).unwrap(), binary);
        assert_eq!(std::fs::metadata(&binary).unwrap().modified().unwrap(), before);
    }
}
