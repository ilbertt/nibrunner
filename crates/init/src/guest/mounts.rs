//! Everything the guest has to mount before a tenant can run.
//!
//! Ported from `apps/runtime/src/mounts.c`. The root is read-only for the life of the VM, so every
//! writable path in the guest is one of these — and every mount point that is not itself on a
//! tmpfs has to already exist in the rootfs image, because nothing can create a directory on a
//! read-only root at boot.

use std::path::Path;
use std::time::{Duration, Instant};

use nix::mount::{mount, MsFlags};

/// A tmpfs is guest memory wearing a filesystem, and one mounted without a size gets half of it.
/// That default is why a tenant filling `/tmp` is OOM-killed rather than told `ENOSPC`: the pages
/// are unevictable with no swap, so the write competes with the heap of the process making it and
/// the killer arrives before the filesystem is full.
///
/// A percentage because guest memory is configurable from 128 MiB to 16 GiB and the kernel
/// resolves the fraction itself; a byte count here would mean reading `/proc/meminfo` to arrive at
/// the same number. Whole ones only — it parses the figure with `memparse` and refuses `size=12.5%`.
///
/// A quarter each — 64 MiB apiece at the 256 MiB default, and the two of them full still leave the
/// tenant half its memory. Room deliberately left: a ceiling low enough to stop a leak early is
/// also low enough to break an app writing honestly to the `TMPDIR` it was handed, and only one of
/// those two is a fault of ours.
///
/// What the room costs is worth knowing, because nothing here is ever emptied. A snapshot is
/// exactly the guest's RAM, so what a tenant leaves in `/tmp` is restored with it on every wake and
/// outlives everything short of a redeploy: memory spent rather than scratch returned, and an app
/// can take months of sleeps to reach a ceiling it would have met in an afternoon.
const TENANT_TMPFS_SIZE: &str = "size=25%";

/// `/app` and `/run` are mode 0755 owned by root, and hold two mount points and a `resolv.conf`.
/// Nothing a tenant does grows them, so they do not scale with the guest.
const RUNTIME_TMPFS_SIZE: &str = "size=1M";

/// Block devices other than the root are not guaranteed to have been probed by the time init runs,
/// and a boot that failed because a node appeared late would be indistinguishable from one whose
/// drive was never attached.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(5);
const DEVICE_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
#[error("{what} could not be mounted at {target}: {reason}")]
pub(crate) struct MountFailed {
    pub what: &'static str,
    pub target: String,
    pub reason: String,
}

/// `mount(2)` answers `EBUSY` when the same filesystem is already mounted at the same place, which
/// for a pseudo-filesystem is the state being asked for.
///
/// Never set for a drive. There `EBUSY` can also mean the block device is held by something else,
/// and carrying on would leave the tenant writing to a tmpfs it thinks is its volume.
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

/// devtmpfs on `/dev`, on its own and first: the rootfs image carries no device nodes, so until
/// this runs there is no `/dev/console` for the kernel or for the runtime to report anything on.
///
/// A kernel built with `CONFIG_DEVTMPFS_MOUNT` has already done it, and `/dev/null` being there is
/// how that is known. Mounting a second one over it would work — devtmpfs has a single instance —
/// but stacking a mount to arrive at the same content is not worth the line. What is below covers
/// the kernel that did not.
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

/// `/proc`, `/sys`, `/run`, `/tmp` and `/dev/shm`.
pub(crate) fn pseudo_filesystems() -> Result<(), MountFailed> {
    let no_privileges = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    let writable = MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
    // Every other mount point here is a directory the image carries. `/dev` is a devtmpfs the
    // kernel populates, so this one has to be made.
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

/// The instance config drive: squashfs, read-only, and nothing on it is ever run.
pub(crate) fn config(device: &str, target: &str) -> Result<(), MountFailed> {
    squashfs("the config drive", device, target, MsFlags::MS_NOEXEC)
}

/// The tenant artifact drive: squashfs, read-only, and the one place exec is allowed.
pub(crate) fn artifact(device: &str, target: &str) -> Result<(), MountFailed> {
    squashfs("the artifact drive", device, target, MsFlags::empty())
}

/// The one writable filesystem the tenant gets, and the only path it owns.
///
/// `noatime`, not the default `relatime`: every atime update is a block write that has to reach an
/// object store underneath the volume.
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
    // The mount point belongs to root and the filesystem inside it to the tenant, because the
    // tenant has to be able to write in it and must not be able to replace it.
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
