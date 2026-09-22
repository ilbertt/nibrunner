use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use protocol::VolumeId;

use crate::adapters::volumes::{filesystem_uuid, VolumeError};
use crate::ports::{CommandRequest, CommandResult, CommandRunner};

const NBD_CLIENT: &str = "nbd-client";

const NBD_CONNECTIONS: u32 = 4;
const NBD_BLOCK_SIZE_BYTES: u32 = 4096;

const NBD_TIMEOUT_SECONDS: u64 = 600;

const SYSFS_BLOCK_DIRECTORY: &str = "/sys/block";
const SYSFS_SECTOR_BYTES: u64 = 512;

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the O_DIRECT read uses it")
)]
const PROBE_BYTES: usize = 4096;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a device is given to let go of its last holder after `nbd-client -d`, and how often
/// that is looked for.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
const RELEASE_POLL: Duration = Duration::from_millis(50);

/// A refused attach is tried once more after this. What refuses it is usually transient — udev
/// opening the device to probe it the moment the kernel let it go, or the server not yet
/// answering on a socket it has only just bound.
const RETRY_PAUSE: Duration = Duration::from_secs(1);

const NO_BYTES: u64 = 0;

/// A device proven to carry the volume it names, and the only thing this adapter will format or
/// hand to a guest. `/dev/nbd<slot>` is a name the slot allocator hands out afresh to whoever
/// holds the slot, while the device goes on serving whatever export it was last attached to: a
/// host that came back without its records once gave every app the device its neighbour's volume
/// was on, and formatted one app's volume over another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenDevice {
    path: String,
    volume_id: VolumeId,
}

impl ProvenDevice {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn volume_id(&self) -> &VolumeId {
        &self.volume_id
    }
}

pub struct NbdDevices {
    sysfs_block: PathBuf,
    commands: Arc<dyn CommandRunner>,
    /// What this daemon attached each device to. The kernel names the export a device serves
    /// only when the client set an identifier for it — `nbd-client -i`, which wants a 3.27
    /// client and a 5.14 kernel, newer than the hosts are installed with — so an attach an
    /// earlier daemon made cannot be read back, and the device has to prove itself by the
    /// filesystem it carries instead.
    attached_to: Mutex<BTreeMap<String, VolumeId>>,
}

impl NbdDevices {
    pub fn new(commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            sysfs_block: PathBuf::from(SYSFS_BLOCK_DIRECTORY),
            commands,
            attached_to: Mutex::new(BTreeMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_sysfs(sysfs_block: PathBuf, commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            sysfs_block,
            commands,
            attached_to: Mutex::new(BTreeMap::new()),
        }
    }

    fn proof(target: &NbdTarget<'_>) -> ProvenDevice {
        ProvenDevice {
            path: target.device_path.to_string(),
            volume_id: target.volume_id.clone(),
        }
    }

    fn remember(&self, target: &NbdTarget<'_>) -> ProvenDevice {
        self.attached_to
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(target.device_path.to_string(), target.volume_id.clone());
        Self::proof(target)
    }

    fn forget(&self, device_path: &str) {
        self.attached_to
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(device_path);
    }

    fn is_known_to_carry(&self, target: &NbdTarget<'_>) -> bool {
        self.attached_to
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(target.device_path)
            == Some(target.volume_id)
    }

    /// The device as this daemon can prove it, touching nothing: it answers reads, and either
    /// this daemon attached it to the export or it carries the same filesystem as the image that
    /// export is served from. A volume no one has formatted yet has no filesystem to be told by
    /// and is never proven here — only an attach by name can say what such a device serves.
    pub async fn proven(&self, target: &NbdTarget<'_>) -> Option<ProvenDevice> {
        if !self.is_usable(target.device_path).await {
            return None;
        }
        if self.is_known_to_carry(target) {
            return Some(Self::proof(target));
        }
        carries_the_image(target).await.then(|| self.remember(target))
    }

    /// A device that cannot prove which volume it carries is taken down and attached to it by
    /// name, which is the one other way to know what it serves.
    pub async fn ready(&self, target: &NbdTarget<'_>) -> Result<ProvenDevice, VolumeError> {
        if let Some(device) = self.proven(target).await {
            return Ok(device);
        }
        tracing::info!(
            device = target.device_path,
            volume_id = %target.volume_id,
            "the device under this app's slot does not prove it carries its volume; attaching it by name"
        );
        self.reattach(target).await
    }

    fn attribute(&self, device_path: &str, attribute: &str) -> Option<String> {
        let name = Path::new(device_path).file_name()?;
        std::fs::read_to_string(self.sysfs_block.join(name).join(attribute)).ok()
    }

    /// Whether the kernel holds a configuration for the device, which is what has to be taken
    /// down before anything else can be attached to it. The pid the kernel writes is the client
    /// that configured the device, and a netlink client exits the moment it has: that pid names
    /// a process that is gone on every attached device, dead or well, so it says nothing about
    /// whether the device answers. Only [`Self::is_usable`] does.
    pub fn is_attached(&self, device_path: &str) -> bool {
        self.attribute(device_path, "pid").is_some()
    }

    pub fn attached_size_bytes(&self, device_path: &str) -> u64 {
        self.attribute(device_path, "size")
            .and_then(|sectors| sectors.trim().parse::<u64>().ok())
            .map_or(NO_BYTES, |sectors| sectors.saturating_mul(SYSFS_SECTOR_BYTES))
    }

    pub async fn is_usable(&self, device_path: &str) -> bool {
        if self.attached_size_bytes(device_path) == NO_BYTES {
            return false;
        }
        reads_first_block(device_path).await
    }

    /// Without `-persist`: the daemon is what reconnects a device whose server went away, and it
    /// needs reads of that device to fail at once so that it can tell. The netlink client the
    /// hosts run ignores the flag; the one being written keeps a process behind to reconnect the
    /// device with a 30 s dead-connection timeout, under which reads block instead.
    pub async fn attach(&self, target: &NbdTarget<'_>) -> Result<ProvenDevice, VolumeError> {
        let timeout = NBD_TIMEOUT_SECONDS.to_string();
        let connections = NBD_CONNECTIONS.to_string();
        let block_size = NBD_BLOCK_SIZE_BYTES.to_string();
        self.client(
            &format!(
                "{NBD_CLIENT} {} -N {}",
                target.device_path,
                target.volume_id.as_str()
            ),
            &[
                NBD_CLIENT,
                "-unix",
                target.socket_path,
                target.device_path,
                "-N",
                target.volume_id.as_str(),
                "-timeout",
                &timeout,
                "-connections",
                &connections,
                "-block-size",
                &block_size,
            ],
        )
        .await?;
        Ok(self.remember(target))
    }

    pub async fn detach(&self, device_path: &str) -> Result<(), VolumeError> {
        self.forget(device_path);
        self.client(
            &format!("{NBD_CLIENT} -d {device_path}"),
            &[NBD_CLIENT, "-d", device_path],
        )
        .await
    }

    /// The client's last line is only ever `Exiting.`; the reason is on the one before, so a
    /// failure carries everything the client said rather than its tail.
    async fn client(&self, named: &str, command: &[&str]) -> Result<(), VolumeError> {
        match self.commands.run(CommandRequest::new(command)).await {
            Ok(result) if result.code == 0 => Ok(()),
            Ok(result) => Err(VolumeError::Unusable(format!(
                "{named} exited {}{}",
                result.code,
                everything_said(&result)
            ))),
            Err(error) => Err(VolumeError::Unusable(error.message())),
        }
    }

    /// Takes the device down if the kernel holds it, then attaches; a refused attach is tried
    /// once more after a pause. A dead device — its server gone, its sockets closed — stays
    /// configured until it is explicitly disconnected, and attaching over it is refused as busy.
    pub async fn reattach(&self, target: &NbdTarget<'_>) -> Result<ProvenDevice, VolumeError> {
        self.take_down(target.device_path).await;
        let refused = match self.attach(target).await {
            Ok(device) => return Ok(device),
            Err(refused) => refused,
        };
        tracing::warn!(
            device = target.device_path,
            error = %refused.message(),
            "the device refused an attach; taking it down and trying once more"
        );
        tokio::time::sleep(RETRY_PAUSE).await;
        self.take_down(target.device_path).await;
        self.attach(target).await
    }

    /// `nbd-client -d` can return before the kernel has let go of the device: the client that
    /// holds it open is only told to stop and closes it in its own time, and the next attach is
    /// refused as busy until it has. Nothing here fails: the attach that follows is what says
    /// whether the device was free.
    async fn take_down(&self, device_path: &str) {
        self.forget(device_path);
        if !self.is_attached(device_path) {
            return;
        }
        if let Err(error) = self.detach(device_path).await {
            tracing::warn!(device = device_path, error = %error.message(), "the device would not be taken down");
        }
        let deadline = tokio::time::Instant::now() + RELEASE_TIMEOUT;
        while self.is_attached(device_path) {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    device = device_path,
                    "the kernel still holds the device after it was taken down"
                );
                return;
            }
            tokio::time::sleep(RELEASE_POLL).await;
        }
    }
}

fn everything_said(result: &CommandResult) -> String {
    let said: Vec<&str> = result
        .stderr
        .lines()
        .chain(result.stdout.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if said.is_empty() {
        String::new()
    } else {
        format!(": {}", said.join(" "))
    }
}

pub struct NbdTarget<'a> {
    pub socket_path: &'a str,
    pub device_path: &'a str,
    pub volume_id: &'a VolumeId,
    /// The file the export is served from, which the host can read as well as the device can. An
    /// export the host holds no file for — a checkpoint, served by a server of its own — has
    /// none, and the device under it is attached by name every time rather than proven.
    pub image_path: Option<&'a Path>,
}

/// Whether the device holds the filesystem that is in the volume's own image. mke2fs gives every
/// filesystem its own uuid, so the device an app was handed carrying its neighbour's volume is
/// told from one carrying its own without writing anything or taking anything down. Neither side
/// is read through `O_DIRECT`: [`NbdDevices::is_usable`] has already proven the device answers,
/// and what is read here was written once, when the volume was formatted.
async fn carries_the_image(target: &NbdTarget<'_>) -> bool {
    let Some(image_path) = target.image_path else {
        return false;
    };
    let device = target.device_path.to_string();
    let image = image_path.display().to_string();
    let read = tokio::task::spawn_blocking(move || {
        filesystem_uuid(&image).is_some_and(|carried| filesystem_uuid(&device) == Some(carried))
    });
    matches!(tokio::time::timeout(PROBE_TIMEOUT, read).await, Ok(Ok(true)))
}

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
    let mut buffer = vec![0u8; PROBE_BYTES * 2];
    let offset = buffer.as_ptr().align_offset(PROBE_BYTES);
    if offset > PROBE_BYTES {
        return false;
    }
    device
        .read_exact(&mut buffer[offset..offset + PROBE_BYTES])
        .is_ok()
}

#[cfg(not(target_os = "linux"))]
fn direct_read(_device_path: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::volumes::tests::image;
    use crate::adapters::volumes::FILESYSTEM_UUID_BYTES;
    use crate::test_support::mocks::{self, CommandLog};

    fn devices(sysfs: &Path) -> (NbdDevices, CommandLog) {
        let (commands, log) = mocks::commands_succeeding();
        (NbdDevices::with_sysfs(sysfs.to_path_buf(), commands), log)
    }

    fn asked(log: &CommandLog) -> Vec<Vec<String>> {
        log.commands()
    }

    fn attribute(sysfs: &Path, device: &str, attribute: &str, value: &str) {
        let directory = sysfs.join(device);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(attribute), value).unwrap();
    }

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

    #[tokio::test]
    async fn a_device_of_no_size_is_unusable_without_the_device_being_opened() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        attribute(sysfs.path(), "nbd0", "size", "0\n");
        assert!(!devices.is_usable("/dev/nbd0").await);
    }

    #[tokio::test]
    async fn an_attach_names_the_export_the_socket_and_the_ceilings_and_does_not_persist() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        devices.attach(&target(&volume_id)).await.unwrap();
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
                "-timeout".into(),
                "600".into(),
                "-connections".into(),
                "4".into(),
                "-block-size".into(),
                "4096".into(),
            ]]
        );
    }

    fn target(volume_id: &VolumeId) -> NbdTarget<'_> {
        NbdTarget {
            socket_path: "/run/zerofs/nbd.sock",
            device_path: "/dev/nbd0",
            volume_id,
            image_path: None,
        }
    }

    fn detaches(command: &[String]) -> bool {
        command.get(1).is_some_and(|word| word == "-d")
    }

    /// What the client prints when the kernel refuses the connect: the reason, then the line it
    /// always ends on.
    fn refused_by_the_kernel() -> Result<crate::ports::CommandResult, crate::ports::CommandError> {
        Ok(crate::ports::CommandResult {
            code: 1,
            stdout: String::new(),
            stderr: "Error: Failed to setup device, check dmesg\nExiting.\n".into(),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_reattach_takes_a_held_device_down_first_and_leaves_an_unheld_one_alone() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        devices.reattach(&target(&volume_id)).await.unwrap();
        assert_eq!(
            asked(&commands).len(),
            1,
            "nothing held it, so nothing to take down"
        );

        attribute(sysfs.path(), "nbd0", "pid", "4123");
        devices.reattach(&target(&volume_id)).await.unwrap();
        let asked = asked(&commands);
        assert_eq!(asked.len(), 3);
        assert_eq!(
            asked[1],
            vec!["nbd-client".to_string(), "-d".into(), "/dev/nbd0".into()]
        );
    }

    #[tokio::test]
    async fn a_detach_whose_pid_lingers_is_waited_for_before_the_attach() {
        let sysfs = tempfile::tempdir().unwrap();
        attribute(sysfs.path(), "nbd0", "pid", "4123");
        let pid_file = sysfs.path().join("nbd0/pid");
        let (commands, log) = mocks::commands_answering(move |request| {
            if detaches(&request.command) {
                // The kernel lets go of the device some time after the client was told to.
                let pid_file = pid_file.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    std::fs::remove_file(pid_file).unwrap();
                });
                Ok(crate::ports::CommandResult::succeeded())
            } else if pid_file.exists() {
                refused_by_the_kernel()
            } else {
                Ok(crate::ports::CommandResult::succeeded())
            }
        });
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands);
        let volume_id = VolumeId::parse("vol-1").unwrap();

        devices.reattach(&target(&volume_id)).await.unwrap();

        let asked = log.commands();
        assert_eq!(asked.len(), 2, "one take-down, one attach: {asked:?}");
        assert!(detaches(&asked[0]));
        assert!(
            !detaches(&asked[1]),
            "the attach waited for the pid to go rather than being refused: {asked:?}"
        );
        assert!(!devices.is_attached("/dev/nbd0"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_pid_that_never_goes_is_not_waited_on_for_ever() {
        let sysfs = tempfile::tempdir().unwrap();
        attribute(sysfs.path(), "nbd0", "pid", "4123");
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        let began = std::time::Instant::now();

        devices.reattach(&target(&volume_id)).await.unwrap();

        assert_eq!(asked(&commands).len(), 2, "the attach was still tried");
        assert!(
            began.elapsed() < RELEASE_TIMEOUT,
            "the wait is on the clock, not the wall"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_attach_is_taken_down_and_tried_once_more() {
        let sysfs = tempfile::tempdir().unwrap();
        attribute(sysfs.path(), "nbd0", "pid", "4123");
        let attaches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = attaches.clone();
        let (commands, log) = mocks::commands_answering(move |request| {
            if detaches(&request.command) {
                return Ok(crate::ports::CommandResult::succeeded());
            }
            if counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                refused_by_the_kernel()
            } else {
                Ok(crate::ports::CommandResult::succeeded())
            }
        });
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands);
        let volume_id = VolumeId::parse("vol-1").unwrap();

        devices.reattach(&target(&volume_id)).await.unwrap();

        let asked: Vec<bool> = log.commands().iter().map(|command| detaches(command)).collect();
        assert_eq!(
            asked,
            vec![true, false, true, false],
            "take down, attach, take down again, attach again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_attach_refused_twice_is_a_failure_that_says_what_the_client_said() {
        let sysfs = tempfile::tempdir().unwrap();
        let (commands, log) = mocks::commands_answering(|request| {
            if detaches(&request.command) {
                Ok(crate::ports::CommandResult::succeeded())
            } else {
                refused_by_the_kernel()
            }
        });
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands);
        let volume_id = VolumeId::parse("vol-1").unwrap();

        let error = devices.reattach(&target(&volume_id)).await.unwrap_err();

        assert_eq!(
            error.message(),
            "the volume could not be made ready: nbd-client /dev/nbd0 -N vol-1 exited 1: \
             Error: Failed to setup device, check dmesg Exiting."
        );
        assert_eq!(
            log.commands().len(),
            2,
            "nothing held the device, so two attaches and no take-down: {:?}",
            log.commands()
        );
    }

    fn refusing() -> Arc<crate::ports::MockCommandRunner> {
        mocks::commands_answering(|request| {
            Err(crate::ports::CommandError::Unstartable {
                executable: request.executable().to_string(),
                reason: "no such file".into(),
            })
        })
        .0
    }

    #[tokio::test]
    async fn a_client_that_will_not_run_leaves_the_volume_unusable_rather_than_thought_attached() {
        let sysfs = tempfile::tempdir().unwrap();
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), refusing());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        let error = devices.attach(&target(&volume_id)).await.unwrap_err();
        assert!(matches!(error, VolumeError::Unusable(_)), "{error}");
        assert!(error.message().contains("nbd-client"), "{error}");
        assert!(devices.detach("/dev/nbd0").await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_will_not_run_leaves_a_reattach_unusable_too() {
        let sysfs = tempfile::tempdir().unwrap();
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), refusing());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        assert!(devices.reattach(&target(&volume_id)).await.is_err());
    }

    #[tokio::test]
    async fn a_client_that_exits_nonzero_is_a_failed_detach_that_says_what_it_printed() {
        let sysfs = tempfile::tempdir().unwrap();
        let (commands, _) = mocks::commands_answering(|_| {
            Ok(crate::ports::CommandResult {
                code: 1,
                stdout: "disconnect, ".into(),
                stderr: "Error: Ioctl failed: Invalid argument\nExiting.\n".into(),
            })
        });
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands);
        let error = devices.detach("/dev/nbd0").await.unwrap_err();
        assert_eq!(
            error.message(),
            "the volume could not be made ready: nbd-client -d /dev/nbd0 exited 1: \
             Error: Ioctl failed: Invalid argument Exiting. disconnect,"
        );
        let volume_id = VolumeId::parse("vol-1").unwrap();
        assert!(devices.attach(&target(&volume_id)).await.is_err());
    }

    #[tokio::test]
    async fn a_client_that_printed_nothing_still_names_the_code_it_exited_with() {
        let sysfs = tempfile::tempdir().unwrap();
        let (commands, _) = mocks::commands_answering(|_| {
            Ok(crate::ports::CommandResult {
                code: 1,
                stdout: String::new(),
                stderr: "\n".into(),
            })
        });
        let devices = NbdDevices::with_sysfs(sysfs.path().to_path_buf(), commands);
        assert_eq!(
            devices.detach("/dev/nbd3").await.unwrap_err().message(),
            "the volume could not be made ready: nbd-client -d /dev/nbd3 exited 1"
        );
    }

    fn reading<'a>(device_path: &'a str, image_path: &'a Path, volume_id: &'a VolumeId) -> NbdTarget<'a> {
        NbdTarget {
            socket_path: "/run/zerofs/nbd.sock",
            device_path,
            volume_id,
            image_path: Some(image_path),
        }
    }

    #[tokio::test]
    async fn a_device_carries_the_export_this_daemon_attached_it_to_until_it_is_taken_down() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        let held = VolumeId::parse("vol-1").unwrap();
        let another = VolumeId::parse("vol-2").unwrap();
        assert!(!devices.is_known_to_carry(&target(&held)));

        devices.attach(&target(&held)).await.unwrap();
        assert!(devices.is_known_to_carry(&target(&held)));
        assert!(
            !devices.is_known_to_carry(&target(&another)),
            "a device carries the one export it was attached to"
        );

        devices.attach(&target(&another)).await.unwrap();
        assert!(
            !devices.is_known_to_carry(&target(&held)),
            "the export the device was last attached to is the one it carries"
        );

        devices.detach("/dev/nbd0").await.unwrap();
        assert!(
            !devices.is_known_to_carry(&target(&another)),
            "a device that has been taken down carries nothing"
        );
    }

    #[tokio::test]
    async fn a_device_that_answers_no_read_is_not_proven_however_well_this_daemon_knows_it() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();
        devices.attach(&target(&volume_id)).await.unwrap();
        assert!(devices.is_known_to_carry(&target(&volume_id)));
        assert!(devices.proven(&target(&volume_id)).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_that_cannot_prove_its_volume_is_attached_by_name_before_it_is_handed_over() {
        let sysfs = tempfile::tempdir().unwrap();
        attribute(sysfs.path(), "nbd0", "pid", "4123");
        let (devices, commands) = devices(sysfs.path());
        let volume_id = VolumeId::parse("vol-1").unwrap();

        let device = devices.ready(&target(&volume_id)).await.unwrap();

        assert_eq!(device.path(), "/dev/nbd0");
        assert_eq!(device.volume_id(), &volume_id);
        let asked = asked(&commands);
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(detaches(&asked[0]));
        assert_eq!(asked[1][4..6], ["-N".to_string(), "vol-1".to_string()]);
    }

    #[tokio::test]
    async fn a_device_holding_the_volumes_own_filesystem_carries_it_and_one_holding_anothers_does_not() {
        let directory = tempfile::tempdir().unwrap();
        let volume_id = VolumeId::parse("vol-1").unwrap();
        let its_own = [1u8; FILESYSTEM_UUID_BYTES];
        let served_from = image(directory.path(), "vol-1", true, its_own);
        let device = image(directory.path(), "nbd0", true, its_own);
        let neighbour = image(directory.path(), "nbd1", true, [2u8; FILESYSTEM_UUID_BYTES]);

        assert!(carries_the_image(&reading(&device, Path::new(&served_from), &volume_id)).await);
        assert!(!carries_the_image(&reading(&neighbour, Path::new(&served_from), &volume_id)).await);
    }

    #[tokio::test]
    async fn a_volume_nothing_has_formatted_yet_is_never_carried_by_a_device_that_only_looks_empty() {
        let directory = tempfile::tempdir().unwrap();
        let volume_id = VolumeId::parse("vol-1").unwrap();
        let uuid = [3u8; FILESYSTEM_UUID_BYTES];
        let bare = image(directory.path(), "vol-1", false, uuid);
        let device = image(directory.path(), "nbd0", false, uuid);
        let neighbour = image(directory.path(), "nbd1", true, uuid);

        assert!(
            !carries_the_image(&reading(&device, Path::new(&bare), &volume_id)).await,
            "two devices with no filesystem on them are not told apart"
        );
        assert!(!carries_the_image(&reading(&neighbour, Path::new(&bare), &volume_id)).await);
        assert!(
            !carries_the_image(&target(&volume_id)).await,
            "an export the host holds no image for cannot be read back at all"
        );
    }

    #[tokio::test]
    async fn a_device_is_read_by_its_name_rather_than_by_the_path_it_was_given_under() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        attribute(sysfs.path(), "nbd0", "size", "8\n");
        assert_eq!(devices.attached_size_bytes("/dev/nbd0"), 8 * 512);
        assert_eq!(devices.attached_size_bytes("/some/other/nbd0"), 8 * 512);
        assert_eq!(devices.attached_size_bytes(""), 0);
        assert!(!devices.is_attached(""));
    }

    #[tokio::test]
    async fn a_device_with_a_size_this_host_cannot_read_a_block_from_is_not_usable() {
        let sysfs = tempfile::tempdir().unwrap();
        let (devices, _) = devices(sysfs.path());
        attribute(sysfs.path(), "nbd-not-a-device", "size", "524288\n");
        assert_eq!(
            devices.attached_size_bytes("/dev/nbd-not-a-device"),
            524_288 * 512
        );
        assert!(!devices.is_usable("/dev/nbd-not-a-device").await);
    }
}
