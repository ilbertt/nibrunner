use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::OnceLock;

use guest_contract::startup::{StartupMilestone, StartupSnapshot};
use nix::sys::time::TimeValLike;

static STARTUP: OnceLock<Checkpoints> = OnceLock::new();
const TIMESTAMP_BYTES: usize = std::mem::size_of::<u64>();

struct Checkpoints(File);

impl Checkpoints {
    fn new() -> std::io::Result<Self> {
        // The control service forks before tenant exec. Share the checkpoints across those
        // processes while closing the descriptor before tenant code can inherit it.
        let descriptor =
            nix::sys::memfd::memfd_create("nibrunner-startup", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)?;
        let file = File::from(descriptor);
        file.set_len((StartupMilestone::ALL.len() * TIMESTAMP_BYTES) as u64)?;
        Ok(Self(file))
    }

    fn record(&self, milestone: StartupMilestone) -> std::io::Result<()> {
        let offset = (milestone.index() * TIMESTAMP_BYTES) as u64;
        let mut previous = [0; TIMESTAMP_BYTES];
        self.0.read_exact_at(&mut previous, offset)?;
        if u64::from_le_bytes(previous) != 0 {
            return Ok(());
        }
        self.0.write_all_at(&monotonic_us()?.to_le_bytes(), offset)
    }

    fn sample(&self) -> Option<StartupSnapshot> {
        let mut milestones = [0; StartupMilestone::ALL.len()];
        for milestone in StartupMilestone::ALL {
            let mut encoded = [0; TIMESTAMP_BYTES];
            self.0
                .read_exact_at(&mut encoded, (milestone.index() * TIMESTAMP_BYTES) as u64)
                .ok()?;
            milestones[milestone.index()] = u64::from_le_bytes(encoded);
        }
        StartupSnapshot::new(monotonic_us().ok()?, milestones)
    }
}

fn monotonic_us() -> std::io::Result<u64> {
    let now = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)?;
    u64::try_from(now.num_microseconds()).map_err(std::io::Error::other)
}

pub(crate) fn initialize() {
    match Checkpoints::new() {
        Ok(checkpoints) => {
            let _ = STARTUP.set(checkpoints);
        }
        Err(error) => super::log(&format!("startup checkpoints are unavailable: {error}")),
    }
    record(StartupMilestone::InitStarted);
}

pub(crate) fn record(milestone: StartupMilestone) {
    if let Some(checkpoints) = STARTUP.get() {
        let _ = checkpoints.record(milestone);
    }
}

pub(crate) fn sample() -> Option<StartupSnapshot> {
    STARTUP.get()?.sample()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_process_sees_the_tenants_checkpoint_after_fork() {
        let _serial = crate::guest::supervisor::tests::one_tenant_at_a_time();
        let checkpoints = Checkpoints::new().unwrap();
        checkpoints.record(StartupMilestone::InitStarted).unwrap();
        checkpoints.record(StartupMilestone::RootReady).unwrap();
        match unsafe { nix::unistd::fork() }.unwrap() {
            nix::unistd::ForkResult::Child => {
                let code = i32::from(checkpoints.record(StartupMilestone::TenantExec).is_err());
                unsafe { libc::_exit(code) }
            }
            nix::unistd::ForkResult::Parent { child } => {
                assert_eq!(
                    nix::sys::wait::waitpid(child, None).unwrap(),
                    nix::sys::wait::WaitStatus::Exited(child, 0)
                );
                let first = checkpoints.sample().unwrap().at(StartupMilestone::TenantExec);
                std::thread::sleep(std::time::Duration::from_millis(1));
                checkpoints.record(StartupMilestone::TenantExec).unwrap();
                assert_eq!(
                    checkpoints.sample().unwrap().at(StartupMilestone::TenantExec),
                    first
                );
            }
        }
        assert_ne!(
            nix::fcntl::fcntl(&checkpoints.0, nix::fcntl::FcntlArg::F_GETFD).unwrap() & libc::FD_CLOEXEC,
            0
        );
    }
}
