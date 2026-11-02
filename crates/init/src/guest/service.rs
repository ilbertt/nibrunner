use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use guest_contract::instance_env::InstanceConfig;
use nix::sys::signal::{kill, SigSet, Signal};
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Pid};

use crate::guest::{cron, log, memory, sqlite, vsock};

const POLL_MS: i32 = 100;
static STOPPING: AtomicBool = AtomicBool::new(false);

pub(super) enum GuestService {
    Cron,
    Sqlite,
}

pub(super) fn stopping() -> bool {
    STOPPING.load(Ordering::Relaxed)
}

extern "C" fn stop_requested(_: libc::c_int) {
    STOPPING.store(true, Ordering::Relaxed);
}

fn prepare_signals() -> nix::Result<()> {
    STOPPING.store(false, Ordering::Relaxed);
    let action = nix::sys::signal::SigAction::new(
        nix::sys::signal::SigHandler::Handler(stop_requested),
        nix::sys::signal::SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        nix::sys::signal::sigaction(Signal::SIGTERM, &action)?;
    }
    let mut blocked = SigSet::empty();
    blocked.add(Signal::SIGTERM);
    blocked.thread_unblock()
}

pub(super) fn ready(fd: RawFd, events: i16, timeout_ms: i32) -> io::Result<bool> {
    let mut watched = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let count = unsafe { libc::poll(&mut watched, 1, timeout_ms) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(count > 0)
}

pub(super) fn stop(pid: Pid) {
    let _ = kill(pid, Signal::SIGTERM);
    wait_for(pid);
}

fn wait_for(pid: Pid) {
    while matches!(waitpid(pid, None), Err(nix::errno::Errno::EINTR)) {}
}

impl GuestService {
    fn name(&self) -> &'static str {
        match self {
            Self::Cron => "cron",
            Self::Sqlite => "SQLite",
        }
    }
    fn port(&self) -> u32 {
        match self {
            Self::Cron => guest_contract::vsock::CRON_EXECUTION_PORT,
            Self::Sqlite => guest_contract::vsock::GUEST_SQLITE_VSOCK_PORT,
        }
    }
    fn capacity(&self) -> usize {
        match self {
            Self::Cron => 64,
            Self::Sqlite => guest_contract::sqlite::MAX_SESSIONS_PER_GUEST,
        }
    }
    fn refuse(&self, fd: RawFd, reason: guest_contract::cron_execution::Rejection) {
        match self {
            Self::Cron => cron::refuse(fd, reason),
            Self::Sqlite => sqlite::refuse(fd),
        }
    }
    fn answer(&self, fd: RawFd, config: &InstanceConfig, ceiling: &memory::Ceiling) {
        match self {
            Self::Cron => cron::answer(fd, config, ceiling),
            Self::Sqlite => sqlite::answer(fd, ceiling),
        }
    }

    pub(super) fn start(&self, config: &InstanceConfig, ceiling: &memory::Ceiling) -> Option<Pid> {
        let parent = nix::unistd::getpid();
        match unsafe { nix::unistd::fork() } {
            Ok(ForkResult::Parent { child }) => Some(child),
            Ok(ForkResult::Child) => {
                if nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)).is_err()
                    || unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } < 0
                    || nix::unistd::getppid() != parent
                    || prepare_signals().is_err()
                {
                    unsafe { libc::_exit(1) }
                }
                self.serve(config, ceiling);
                unsafe { libc::_exit(0) }
            }
            Err(error) => {
                log(&format!(
                    "the {} channel could not be started: {error}",
                    self.name()
                ));
                None
            }
        }
    }

    fn serve(&self, config: &InstanceConfig, ceiling: &memory::Ceiling) {
        let Ok(listener) = vsock::listener(self.port()) else {
            log(&format!("the {} port could not be opened", self.name()));
            return;
        };
        let _ = nix::fcntl::fcntl(
            &listener,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        );
        let mut workers = std::collections::BTreeSet::new();
        while !stopping() {
            loop {
                let mut status = 0;
                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if pid <= 0 {
                    break;
                }
                workers.remove(&Pid::from_raw(pid));
            }
            if !ready(listener.as_raw_fd(), libc::POLLIN, POLL_MS).unwrap_or(false) {
                continue;
            }
            let Ok(connection) = vsock::accept_from_host(&listener) else {
                continue;
            };
            let _ = nix::fcntl::fcntl(
                &connection,
                nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
            );
            if workers.len() >= self.capacity() {
                self.refuse(
                    connection.as_raw_fd(),
                    guest_contract::cron_execution::Rejection::Busy,
                );
                continue;
            }
            let mut blocked = SigSet::empty();
            blocked.add(Signal::SIGTERM);
            let _ = blocked.thread_block();
            let parent = nix::unistd::getpid();
            match unsafe { nix::unistd::fork() } {
                Ok(ForkResult::Parent { child }) => {
                    workers.insert(child);
                }
                Ok(ForkResult::Child) => {
                    drop(listener);
                    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } < 0
                        || nix::unistd::getppid() != parent
                        || prepare_signals().is_err()
                    {
                        unsafe { libc::_exit(1) }
                    }
                    self.answer(connection.as_raw_fd(), config, ceiling);
                    unsafe { libc::_exit(0) }
                }
                Err(_) => self.refuse(
                    connection.as_raw_fd(),
                    guest_contract::cron_execution::Rejection::SpawnFailed,
                ),
            }
            let _ = blocked.thread_unblock();
        }
        for pid in &workers {
            let _ = kill(*pid, Signal::SIGTERM);
        }
        for pid in workers {
            wait_for(pid);
        }
    }
}
