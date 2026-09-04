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
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);
const DEVICE_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, thiserror::Error)]
#[error("{what} could not be mounted at {target}: {reason}")]
pub(crate) struct MountFailed {
    pub what: &'static str,
    pub target: String,
    pub reason: String,
}

fn mounted(
    what: &'static str,
    source: Option<&str>,
    target: &str,
    filesystem: Option<&str>,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<(), MountFailed> {
    mount(source, Path::new(target), filesystem, flags, data).map_err(|error| MountFailed {
        what,
        target: target.to_string(),
        reason: error.to_string(),
    })
}

/// devtmpfs on `/dev`, on its own and first: the rootfs image carries no device nodes, so until
/// this runs there is no `/dev/console` for the kernel or for the runtime to report anything on.
pub(crate) fn dev() -> Result<(), MountFailed> {
    mounted(
        "devtmpfs",
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        None,
    )
}

pub(crate) fn pseudo_filesystems() -> Result<(), MountFailed> {
    let no_privileges = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    mounted("proc", Some("proc"), "/proc", Some("proc"), no_privileges, None)?;
    mounted("sysfs", Some("sysfs"), "/sys", Some("sysfs"), no_privileges, None)?;
    tmpfs("/run", RUNTIME_TMPFS_SIZE)?;
    tmpfs("/tmp", TENANT_TMPFS_SIZE)?;
    tmpfs("/dev/shm", TENANT_TMPFS_SIZE)
}

pub(crate) fn tmpfs(target: &str, options: &str) -> Result<(), MountFailed> {
    mounted(
        "a tmpfs",
        Some("tmpfs"),
        target,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(options),
    )
}

/// The instance config drive: squashfs, read-only, and nothing on it is ever run.
pub(crate) fn config(device: &str, target: &str) -> Result<(), MountFailed> {
    wait_for_device(device)?;
    mounted(
        "the config drive",
        Some(device),
        target,
        Some("squashfs"),
        MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None,
    )
}

/// The tenant artifact drive: squashfs, read-only, and the one place exec is allowed.
pub(crate) fn artifact(device: &str, target: &str) -> Result<(), MountFailed> {
    wait_for_device(device)?;
    mounted(
        "the artifact drive",
        Some(device),
        target,
        Some("squashfs"),
        MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        None,
    )
}

/// The one writable filesystem the tenant gets, and the only path it owns.
///
/// `nodev`, `nosuid` and `noexec`: a tenant's data is data. A binary a tenant wrote to their own
/// disk and then ran would be a deploy nothing on the host has a digest for.
pub(crate) fn tenant_data(device: &str, target: &str, uid: u32, gid: u32) -> Result<(), MountFailed> {
    wait_for_device(device)?;
    mounted(
        "the data drive",
        Some(device),
        target,
        Some("ext4"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None,
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
        reason: error.to_string(),
    })
}

fn wait_for_device(path: &str) -> Result<(), MountFailed> {
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    while Instant::now() < deadline {
        if Path::new(path).exists() {
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
