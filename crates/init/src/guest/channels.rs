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
    match unsafe { nix::unistd::fork() } {
        Ok(ForkResult::Parent { child }) => Some(child),
        Ok(ForkResult::Child) => {
            // The guest blocks SIGTERM before forking us; left blocked, Channels::stop() would
            // SIGTERM this child and then waitpid on it forever, so the guest never reboots.
            let _ = nix::sys::signal::SigSet::all().thread_unblock();
            serve()
        }
        Err(error) => {
            log(&format!("the {what} channel could not be started: {error}"));
            None
        }
    }
}
