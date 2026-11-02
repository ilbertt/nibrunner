//! The cgroup the tenant's ceiling is enforced through. What the ceiling is and what thrashing
//! looks like are `crate::ceiling`, which has no Linux in it and is tested everywhere.

use std::ffi::CString;
use std::path::{Path, PathBuf};

use crate::ceiling::{ceiling_for, field, Reading};
use crate::guest::mounts::MountFailed;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const WORKLOAD_CGROUP: &str = "/sys/fs/cgroup/workload";
const SQLITE_MEMORY_BYTES: u64 = 24 * 1024 * 1024;

pub(super) fn workload_file(name: &str) -> PathBuf {
    Path::new(WORKLOAD_CGROUP).join(name)
}
fn tenant_cgroup() -> PathBuf {
    Path::new(WORKLOAD_CGROUP).join("tenant")
}
fn tenant_file(name: &str) -> PathBuf {
    tenant_cgroup().join(name)
}

fn create_group(path: &Path, limit: u64, oom_group: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    for (name, value) in [
        ("memory.max", limit.to_string()),
        ("memory.swap.max", "0".into()),
        ("memory.oom.group", oom_group.into()),
    ] {
        std::fs::write(path.join(name), value)?;
    }
    Ok(())
}

pub(super) fn join_sqlite(ceiling: &Ceiling) -> Result<(), String> {
    let current = std::fs::read_to_string(workload_file("memory.current"))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .ok_or("the workload memory budget could not be read")?;
    if ceiling.limit_bytes.saturating_sub(current) < SQLITE_MEMORY_BYTES {
        return Err("the guest has no memory available for a database reader".into());
    }
    let group = Path::new(WORKLOAD_CGROUP).join("sqlite");
    create_group(&group, SQLITE_MEMORY_BYTES, "0").map_err(|error| error.to_string())?;
    // Readers share the app's existing budget; memory pressure must prefer a reader over the app.
    std::fs::write("/proc/self/oom_score_adj", "1000").map_err(|error| error.to_string())?;
    let path = CString::new(group.join("cgroup.procs").to_string_lossy().as_bytes())
        .map_err(|error| error.to_string())?;
    join(&path).map_err(|error| error.to_string())
}

#[derive(Debug, Clone)]
pub(crate) struct Ceiling {
    pub limit_bytes: u64,
    procs_file: CString,
}

impl Ceiling {
    /// The file the tenant writes its own pid into, prepared here because the write happens
    /// between fork and exec, where nothing may allocate.
    pub(crate) fn procs_file(&self) -> &CString {
        &self.procs_file
    }
}

pub(crate) fn mount() -> Result<(), MountFailed> {
    crate::guest::mounts::cgroup2(CGROUP_ROOT)
}

/// The cgroup the tenant will be put in, limited before anything is in it. A kill takes every
/// process in it together: a tenant that forks would otherwise limp on without the one that
/// held its memory.
pub(crate) fn prepare(guest_total_bytes: u64) -> Result<Ceiling, String> {
    let limit_bytes = ceiling_for(guest_total_bytes);
    let unwritable = |path: &str, error: std::io::Error| format!("{path} could not be written: {error}");

    let subtree_control = format!("{CGROUP_ROOT}/cgroup.subtree_control");
    std::fs::write(&subtree_control, "+memory").map_err(|error| unwritable(&subtree_control, error))?;
    create_group(Path::new(WORKLOAD_CGROUP), limit_bytes, "0").map_err(|error| error.to_string())?;
    std::fs::write(workload_file("cgroup.subtree_control"), "+memory").map_err(|error| error.to_string())?;
    create_group(&tenant_cgroup(), limit_bytes, "1").map_err(|error| error.to_string())?;
    let procs_file = CString::new(tenant_file("cgroup.procs").to_string_lossy().as_bytes())
        .map_err(|_| "the cgroup path holds a nul byte".to_string())?;
    Ok(Ceiling {
        limit_bytes,
        procs_file,
    })
}

/// Puts the calling process into the tenant's cgroup. Called by the forked child before it
/// execs, so it uses nothing that allocates: the path was made into a C string before the fork
/// and the pid is formatted on the stack.
pub(crate) fn join(procs_file: &CString) -> nix::Result<()> {
    let pid = nix::unistd::getpid().as_raw();
    let mut digits = [0u8; 20];
    let mut end = digits.len();
    let mut rest = pid.unsigned_abs();
    loop {
        end -= 1;
        digits[end] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let fd = nix::fcntl::open(
        procs_file.as_c_str(),
        nix::fcntl::OFlag::O_WRONLY | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )?;
    let written = nix::unistd::write(&fd, &digits[end..]);
    drop(fd);
    written.map(|_| ())
}

/// Every process in the cgroup, with SIGKILL, at once.
pub(crate) fn kill_everything() -> std::io::Result<()> {
    std::fs::write(tenant_file("cgroup.kill"), "1")
}

pub(crate) fn read() -> Option<Reading> {
    let value = |name: &str| std::fs::read_to_string(tenant_file(name)).ok();
    Some(Reading {
        current_bytes: value("memory.current")?.trim().parse().ok()?,
        major_faults: field(&value("memory.stat")?, "pgmajfault")?,
        oom_kills: field(&value("memory.events")?, "oom_kill")?,
    })
}
