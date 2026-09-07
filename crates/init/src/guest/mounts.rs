use std::path::Path;
use std::time::{Duration, Instant};

use nix::mount::{mount, MsFlags};

const TENANT_TMPFS_SIZE: &str = "size=25%";

const RUNTIME_TMPFS_SIZE: &str = "size=1M";

const DEVICE_TIMEOUT: Duration = Duration::from_secs(5);
const DEVICE_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
#[error("{what} could not be mounted at {target}: {reason}")]
pub(crate) struct MountFailed {
    pub what: &'static str,
    pub target: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Existing {
    Tolerate,
    Refuse,
}

fn mounted(
    what: &'static str,
    source: &str,
    target: &str,
    filesystem: &str,
    flags: MsFlags,
    data: Option<&str>,
    existing: Existing,
) -> Result<(), MountFailed> {
    match mount(Some(source), Path::new(target), Some(filesystem), flags, data) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::EBUSY) if existing == Existing::Tolerate => {
            crate::guest::log(&format!("{filesystem} was already mounted on {target}"));
            Ok(())
        }
        Err(error) => Err(MountFailed {
            what,
            target: target.to_string(),
            reason: error.to_string(),
        }),
    }
}

fn ensure_directory(path: &str, mode: u32) -> Result<(), MountFailed> {
    match nix::unistd::mkdir(Path::new(path), nix::sys::stat::Mode::from_bits_truncate(mode)) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => Ok(()),
        Err(error) => Err(MountFailed {
            what: "a directory",
            target: path.to_string(),
            reason: error.to_string(),
        }),
    }
}

pub(crate) fn dev() -> Result<(), MountFailed> {
    if Path::new("/dev/null").exists() {
        return Ok(());
    }
    mounted(
        "devtmpfs",
        "devtmpfs",
        "/dev",
        "devtmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("mode=0755"),
        Existing::Tolerate,
    )
}

pub(crate) fn pseudo_filesystems() -> Result<(), MountFailed> {
    let no_privileges = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    let writable = MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
    ensure_directory("/dev/shm", 0o1777)?;

    mounted(
        "proc",
        "proc",
        "/proc",
        "proc",
        no_privileges,
        None,
        Existing::Tolerate,
    )?;
    mounted(
        "sysfs",
        "sysfs",
        "/sys",
        "sysfs",
        no_privileges,
        None,
        Existing::Tolerate,
    )?;
    mounted(
        "a tmpfs",
        "tmpfs",
        "/run",
        "tmpfs",
        writable,
        Some(&format!("mode=0755,{RUNTIME_TMPFS_SIZE}")),
        Existing::Refuse,
    )?;
    mounted(
        "a tmpfs",
        "tmpfs",
        "/tmp",
        "tmpfs",
        writable,
        Some(&format!("mode=1777,{TENANT_TMPFS_SIZE}")),
        Existing::Refuse,
    )?;
    mounted(
        "a tmpfs",
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        writable,
        Some(&format!("mode=1777,{TENANT_TMPFS_SIZE}")),
        Existing::Refuse,
    )
}

pub(crate) fn tmpfs(target: &str, options: &str) -> Result<(), MountFailed> {
    mounted(
        "a tmpfs",
        "tmpfs",
        target,
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(options),
        Existing::Refuse,
    )
}

fn squashfs(what: &'static str, device: &str, target: &str, extra: MsFlags) -> Result<(), MountFailed> {
    wait_for_device(device)?;
    ensure_directory(target, 0o755)?;
    mounted(
        what,
        device,
        target,
        "squashfs",
        MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV | extra,
        None,
        Existing::Refuse,
    )
}

pub(crate) fn config(device: &str, target: &str) -> Result<(), MountFailed> {
    squashfs("the config drive", device, target, MsFlags::MS_NOEXEC)
}

pub(crate) fn artifact(device: &str, target: &str) -> Result<(), MountFailed> {
    squashfs("the artifact drive", device, target, MsFlags::empty())
}

pub(crate) fn tenant_data(device: &str, target: &str, uid: u32, gid: u32) -> Result<(), MountFailed> {
    wait_for_device(device)?;
    ensure_directory(target, 0o755)?;
    mounted(
        "the data drive",
        device,
        target,
        "ext4",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOATIME,
        None,
        Existing::Refuse,
    )?;
    nix::unistd::chown(
        Path::new(target),
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .map_err(|error| MountFailed {
        what: "the data drive",
        target: target.to_string(),
        reason: format!("it could not be given to uid {uid}: {error}"),
    })
}

fn wait_for_device(path: &str) -> Result<(), MountFailed> {
    let started = Instant::now();
    let deadline = started + DEVICE_TIMEOUT;
    while Instant::now() < deadline {
        if Path::new(path).exists() {
            if started.elapsed() > DEVICE_POLL {
                crate::guest::log(&format!(
                    "{path} appeared after {}ms",
                    started.elapsed().as_millis()
                ));
            }
            return Ok(());
        }
        std::thread::sleep(DEVICE_POLL);
    }
    Err(MountFailed {
        what: "a drive",
        target: path.to_string(),
        reason: "it never appeared, so it was never attached".to_string(),
    })
}
