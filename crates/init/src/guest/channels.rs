//! The two vsock ports the host asks the guest about its own filesystem on.
//!
//! Each is answered in a child of PID 1 rather than on the supervisor's loop, which does not run
//! while the tenant is between restarts — and a host asking about a filesystem should not have to
//! know whether the tenant happens to be up. Failing to start is not fatal: the tenant still runs,
//! and an export or a browse that cannot reach the guest fails on the host, where somebody can see
//! it.
//!
//! The guest kernel is built without `CONFIG_VSOCKETS_LOOPBACK`, so the tenant cannot reach either
//! listener — the only peer a guest vsock port has is the host.

use nix::unistd::{ForkResult, Pid};

use crate::guest::{control, filesystem, log};

pub(crate) struct Channels {
    control: Option<Pid>,
    files: Option<Pid>,
}

pub(crate) fn start() -> Channels {
    Channels {
        control: fork_channel("control", control::serve),
        files: fork_channel("filesystem", filesystem::serve),
    }
}

impl Channels {
    /// Ends both and leaves the filesystem thawed. Both halves are needed: a freeze is superblock
    /// state that outlives whoever asked for it, so killing the process does not lift it, and
    /// unmounting a frozen filesystem blocks for as long as it stays frozen.
    pub(crate) fn stop(&self) {
        for pid in [self.control, self.files].into_iter().flatten() {
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        }
        control::thaw_quietly();
        for pid in [self.control, self.files].into_iter().flatten() {
            let _ = nix::sys::wait::waitpid(pid, None);
        }
    }
}

fn fork_channel(what: &'static str, serve: fn() -> !) -> Option<Pid> {
    // Safety: the child execs nothing and calls only what a forked child of a single-threaded
    // process may — it goes straight into its own accept loop.
    match unsafe { nix::unistd::fork() } {
        Ok(ForkResult::Parent { child }) => Some(child),
        Ok(ForkResult::Child) => serve(),
        Err(error) => {
            log(&format!("the {what} channel could not be started: {error}"));
            None
        }
    }
}
