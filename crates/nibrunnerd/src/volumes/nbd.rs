//! The kernel's end of a volume served over NBD, and the only part of one that answers while its
//! server does not.
//!
//! Ported from `apps/agent/src/lib/volumes/nbd.ts`. Everything here exists because of one failure
//! mode: opening `/dev/nbdN` is what blocks. A ZeroFS restart leaves the kernel holding a device
//! whose socket is gone, and every read queues behind requests that will not complete until the
//! request timeout below. Anything that has to answer *about* a device — is it attached, how big
//! is it — is therefore read from sysfs, which is answered from memory and cannot queue behind
//! anything.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::VolumeId;

use crate::services::{CommandRequest, CommandRunner};
use crate::volumes::VolumeError;

const NBD_CLIENT: &str = "nbd-client";

const NBD_CONNECTIONS: u32 = 4;
const NBD_BLOCK_SIZE_BYTES: u32 = 4096;

/// S3 round trips under load turn a short timeout into an EIO the guest's ext4 answers by
/// remounting read-only.
///
/// It is also the only ceiling anything on this host has over a dead export. A request the kernel
/// has accepted cannot be taken back by a signal — whatever waits on it sleeps uninterruptibly,
/// past SIGKILL and past systemd — so this number, and not any timeout in userspace, is what
/// eventually frees a waiter. Nothing below waits on one, which is what lets this stay long
/// enough to be safe for the guest.
const NBD_TIMEOUT_SECONDS: u64 = 600;

const SYSFS_BLOCK_DIRECTORY: &str = "/sys/block";
/// The unit the kernel publishes capacity in, whatever logical block size the device negotiated.
const SYSFS_SECTOR_BYTES: u64 = 512;

/// One block, at the offset every filesystem on the device keeps something at.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the O_DIRECT read uses it")
)]
const PROBE_BYTES: usize = 4096;

/// A dead device answers instantly and a live one answers from cache, so this bounds the case
/// neither covers: a server that is up but reaching S3 for the block. Shorter than the ceiling on
/// the device itself, because a probe that waited that long would hold the reconcile behind it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

const NO_BYTES: u64 = 0;

pub struct NbdDevices {
    sysfs_block: PathBuf,
    commands: Arc<dyn CommandRunner>,
}

impl NbdDevices {
    pub fn new(commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            sysfs_block: PathBuf::from(SYSFS_BLOCK_DIRECTORY),
            commands,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_sysfs(sysfs_block: PathBuf, commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            sysfs_block,
            commands,
        }
    }

    /// An ordinary sysfs attribute on the gendisk, reached without opening the device.
    fn attribute(&self, device_path: &str, attribute: &str) -> Option<String> {
        let name = Path::new(device_path).file_name()?;
        std::fs::read_to_string(self.sysfs_block.join(name).join(attribute)).ok()
    }

    /// Whether a device has a client, which is a different question from whether it has a working
    /// one. Kept because a detach is only worth attempting against a device somebody is holding.
    ///
    /// The kernel creates `pid` when a client takes the device and removes it when one lets go, so
    /// the file being there is the whole answer. It is the same file `nbd-client -check` reads,
    /// reached without a process this host would then have to be able to kill.
    pub fn is_attached(&self, device_path: &str) -> bool {
        self.attribute(device_path, "pid").is_some()
    }

    /// Zero for a device nothing is attached to, which is the state a reboot leaves every one of
    /// them in — and the state `nbd-client -d` puts one back into, so a device this host has given
    /// up on stops being probed from the pass after it gave up.
    pub fn attached_size_bytes(&self, device_path: &str) -> u64 {
        self.attribute(device_path, "size")
            .and_then(|sectors| sectors.trim().parse::<u64>().ok())
            .map_or(NO_BYTES, |sectors| sectors.saturating_mul(SYSFS_SECTOR_BYTES))
    }

    /// Whether the device answers, which is what the kernel's own record of it does not say.
    ///
    /// A ZeroFS restart leaves the kernel holding a device that reports its size and names its
    /// client and fails every read: sysfs is right about both, and the guest's ext4 cannot read
    /// its own superblock. Observed on a live host, on both ZeroFS versions, so it is the
    /// reconnect and not the release. Liveness has to be a read, because a read is the thing that
    /// is broken.
    ///
    /// The size is checked first, and it is not belt-and-braces. A detached nbd device is zero
    /// bytes long, and reading one block of a zero-length device is not an error — it returns no
    /// bytes and succeeds. Asking only whether the read succeeded therefore answers yes for a
    /// device that is not attached at all, which is every device on a host that has just rebooted,
    /// and a host whose volumes are then never attached because nothing believes anything is wrong.
    ///
    /// Doing the size from sysfs is what keeps the common case off the device: a host whose
    /// volumes are detached opens nothing at all, and only a device the kernel says is carrying
    /// data is ever read.
    pub async fn is_usable(&self, device_path: &str) -> bool {
        if self.attached_size_bytes(device_path) == NO_BYTES {
            return false;
        }
        reads_first_block(device_path).await
    }

    /// `-persist` reconnects on its own, and it is kept for the drops it does cover — a socket
    /// that goes and comes back while the kernel still has the device. It does not cover a ZeroFS
    /// restart: the kernel tears the device down first, and what reconnects onto it reads as
    /// attached and answers every read with an error. `is_usable` is what notices that, and
    /// `reattach` is what repairs it.
    pub async fn attach(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        self.connect(target, &["-persist"]).await
    }

    /// A checkpoint server, which is a different kind of peer from the one behind a tenant's disk.
    ///
    /// No `-persist`: the server is started for one export and stopped after it, so one that died
    /// mid-read is not coming back, and reconnecting forever would turn that into an export that
    /// hangs until its own ceiling instead of one that says what went wrong. No guest is attached
    /// here either, so there is no ext4 to remount read-only while it waits.
    ///
    /// Nothing asks for read-only. A checkpoint server is always read-only, so it advertises the
    /// export that way and the kernel marks the device — which holds whether or not this side
    /// remembered to ask, unlike a flag.
    pub async fn attach_checkpoint(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        self.connect(target, &[]).await
    }

    async fn connect(&self, target: &NbdTarget<'_>, extra: &[&str]) -> Result<(), VolumeError> {
        let timeout = NBD_TIMEOUT_SECONDS.to_string();
        let connections = NBD_CONNECTIONS.to_string();
        let block_size = NBD_BLOCK_SIZE_BYTES.to_string();
        let mut command = vec![
            NBD_CLIENT,
            "-unix",
            target.socket_path,
            target.device_path,
            "-N",
            target.volume_id.as_str(),
        ];
        command.extend_from_slice(extra);
        command.extend_from_slice(&[
            "-timeout",
            &timeout,
            "-connections",
            &connections,
            "-block-size",
            &block_size,
        ]);
        self.commands
            .stdout_of(CommandRequest::new(&command))
            .await
            .map(|_| ())
            .map_err(|error| VolumeError::Unusable(error.message()))
    }

    /// Two ioctls on a device somebody already has open, which is why this works where a read does
    /// not: `NBD_DISCONNECT` and `NBD_CLEAR_SOCK` error every request the kernel is holding rather
    /// than joining the queue behind them. It is what frees anything sleeping on a dead device,
    /// and the reason a wedged host recovers instead of waiting out the request timeout.
    pub async fn detach(&self, device_path: &str) -> Result<(), VolumeError> {
        self.commands
            .run(CommandRequest::new(&[NBD_CLIENT, "-d", device_path]))
            .await
            .map(|_| ())
            .map_err(|error| VolumeError::Unusable(error.message()))
    }

    /// Takes the device down before bringing it up, because the failure this repairs is a device
    /// the kernel still holds. Attaching over it would find the minor busy; only a detach frees it.
    ///
    /// The detach is allowed to fail: the same call has to serve a device nobody has ever attached,
    /// where there is nothing to take down and `-d` says so.
    pub async fn reattach(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        if self.is_attached(target.device_path) {
            let _ = self.detach(target.device_path).await;
        }
        self.attach(target).await
    }
}

/// One device, one export, one server. Grouped because none of the three means anything without
/// the other two: an export name against the wrong socket attaches a different tenant's disk.
pub struct NbdTarget<'a> {
    pub socket_path: &'a str,
    pub device_path: &'a str,
    pub volume_id: &'a VolumeId,
}

/// The one thing here that opens the device, and so the one thing that can be left behind.
///
/// `O_DIRECT` is what makes it a read of the device rather than of the page cache. Without it the
/// host answers out of memory for a device that has been dead for hours, which is the same false
/// yes this exists to replace.
///
/// The read runs on a blocking thread this side gives up on rather than waits for, because a read
/// the kernel has accepted cannot be cancelled: the timeout is the moment this stops waiting and
/// not the moment the read gives up. What bounds the threads left behind is the repair the `false`
/// triggers — a device judged unusable is reattached, and the `nbd-client -d` that starts the
/// reattach errors every queued request on that device, which frees the read waiting on one. After
/// that its size reads zero and nothing probes it again until an attach succeeds, so the count is
/// bounded by the minors this host was given rather than by how often the reconcile runs.
async fn reads_first_block(device_path: &str) -> bool {
    let path = device_path.to_string();
    let read = tokio::task::spawn_blocking(move || direct_read(&path));
    matches!(tokio::time::timeout(PROBE_TIMEOUT, read).await, Ok(Ok(true)))
}

#[cfg(target_os = "linux")]
fn direct_read(device_path: &str) -> bool {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let Ok(mut device) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(device_path)
    else {
        return false;
    };
    // `O_DIRECT` reads into a buffer the block layer can DMA to, which means an aligned one. Twice
    // the block is asked for so that an aligned window is guaranteed to exist inside it.
    let mut buffer = vec![0u8; PROBE_BYTES * 2];
    let offset = buffer.as_ptr().align_offset(PROBE_BYTES);
    if offset > PROBE_BYTES {
        return false;
    }
    device
        .read_exact(&mut buffer[offset..offset + PROBE_BYTES])
        .is_ok()
}

/// Off Linux there is no `O_DIRECT` and no `/dev/nbd*`; a host that cannot serve one never asks.
#[cfg(not(target_os = "linux"))]
fn direct_read(_device_path: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::RecordingCommandRunner;

    fn devices(sysfs: &Path) -> (NbdDevices, Arc<RecordingCommandRunner>) {
        let commands = RecordingCommandRunner::succeeding();
        (
            NbdDevices::with_sysfs(sysfs.to_path_buf(), commands.clone()),
            commands,
        )
    }

    fn asked(commands: &RecordingCommandRunner) -> Vec<Vec<String>> {
        commands.calls().into_iter().map(|call| call.command).collect()
    }

    fn attribute(sysfs: &Path, device: &str, attribute: &str, value: &str) {
        let directory = sysfs.join(device);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(attribute), value).unwrap();
    }

    /// The kernel writes `pid` when a client takes the device, so the file is the whole answer —
    /// and reaching for it costs nothing on a device whose server has gone.
    #[tokio::test]
    async fn a_device_is_attached_when_the_kernel_says_something_holds_it() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        assert!(!devices.is_attached("/dev/nbd0"));
        attribute(sysfs.path(), "nbd0", "pid", "4123\n");
        assert!(devices.is_attached("/dev/nbd0"));
    }

    #[tokio::test]
    async fn size_is_sectors_and_a_device_nothing_holds_is_zero() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        assert_eq!(devices.attached_size_bytes("/dev/nbd0"), 0);
        attribute(sysfs.path(), "nbd0", "size", "524288\n");
        assert_eq!(devices.attached_size_bytes("/dev/nbd0"), 524_288 * 512);
        attribute(sysfs.path(), "nbd1", "size", "not a number");
        assert_eq!(devices.attached_size_bytes("/dev/nbd1"), 0);
    }

    /// Reading one block of a zero-length device succeeds and returns nothing, so a probe that
    /// asked only whether the read worked would call every device on a freshly rebooted host
    /// healthy — and nothing would ever attach them.
    #[tokio::test]
    async fn a_device_of_no_size_is_unusable_without_the_device_being_opened() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        attribute(sysfs.path(), "nbd0", "size", "0\n");
        // `/dev/nbd0` may or may not exist on the machine running this; the assertion is that
        // neither case is reached, because sysfs already answered.
        assert!(!devices.is_usable("/dev/nbd0").await);
    }

    #[tokio::test]
    async fn an_attach_names_the_export_the_socket_and_the_ceilings() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        devices
            .attach(&NbdTarget {
                socket_path: "/run/zerofs/nbd.sock",
                device_path: "/dev/nbd0",
                volume_id: &volume_id,
            })
            .await
            .unwrap();
        let asked = asked(&commands);
        assert_eq!(
            asked,
            vec![vec![
                "nbd-client".to_string(),
                "-unix".into(),
                "/run/zerofs/nbd.sock".into(),
                "/dev/nbd0".into(),
                "-N".into(),
                "vol-1".into(),
                "-persist".into(),
                "-timeout".into(),
                "600".into(),
                "-connections".into(),
                "4".into(),
                "-block-size".into(),
                "4096".into(),
            ]]
        );
    }

    /// A checkpoint server is started for one export and stopped after it, so reconnecting for
    /// ever would turn a server that died into an export that hangs.
    #[tokio::test]
    async fn a_checkpoint_is_attached_without_persist() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        devices
            .attach_checkpoint(&NbdTarget {
                socket_path: "/run/zerofs-checkpoint/one/nbd.sock",
                device_path: "/dev/nbd63",
                volume_id: &volume_id,
            })
            .await
            .unwrap();
        assert!(!asked(&commands)[0].contains(&"-persist".to_string()));
    }

    /// The minor would be busy otherwise: the failure this repairs is a device the kernel still
    /// holds, and only a detach frees one.
    #[tokio::test]
    async fn a_reattach_takes_a_held_device_down_first_and_leaves_an_unheld_one_alone() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        let target = NbdTarget {
            socket_path: "/run/zerofs/nbd.sock",
            device_path: "/dev/nbd0",
            volume_id: &volume_id,
        };
        devices.reattach(&target).await.unwrap();
        assert_eq!(
            asked(&commands).len(),
            1,
            "nothing held it, so nothing to take down"
        );

        attribute(sysfs.path(), "nbd0", "pid", "4123");
        devices.reattach(&target).await.unwrap();
        let asked = asked(&commands);
        assert_eq!(asked.len(), 3);
        assert_eq!(
            asked[1],
            vec!["nbd-client".to_string(), "-d".into(), "/dev/nbd0".into()]
        );
    }
}
