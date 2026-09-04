//! Starting the tenant, watching it, and reaping whatever else dies.
//!
//! Ported from `apps/runtime/src/supervise.c`. Every child that dies is reaped, tenant or not —
//! nobody else in the guest will.

use std::ffi::CString;
use std::time::{Duration, Instant};

use guest_contract::instance_env::InstanceConfig;
use guest_contract::paths;
use nix::sys::signal::{SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{ForkResult, Gid, Pid, Uid};

use crate::guest::log;
use crate::guest::logs::Forwarder;
use crate::supervise::{backoff_ms, budget_resets, Outcome, SHUTDOWN_GRACE_MS};

pub(crate) use crate::supervise::Outcome as Ended;

/// Blocked rather than handled, so nothing is delivered between the fork and the exec and every
/// arrival is taken deliberately by the loop below.
///
/// Must be called before the first fork, and early enough that a shutdown arriving during boot is
/// still honoured: PID 1 discards signals it has neither blocked nor handled, so an unblocked
/// SIGINT before this point is gone for good.
pub(crate) fn block_signals() {
    let mut blocked = SigSet::empty();
    blocked.add(Signal::SIGINT);
    blocked.add(Signal::SIGTERM);
    blocked.add(Signal::SIGCHLD);
    let _ = blocked.thread_block();
}

fn waited_signals() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGCHLD);
    set
}

pub(crate) fn supervise(config: &InstanceConfig) -> Ended {
    let mut restarts = 0u32;
    let mut forwarder = Forwarder::new();
    loop {
        let started = Instant::now();
        let Some(started_tenant) = spawn(config) else {
            return Outcome::SpawnFailed;
        };
        let Tenant { pid: tenant, output } = started_tenant;
        match watch(tenant, output, &mut forwarder) {
            Watched::ShutdownRequested => {
                stop(tenant);
                return Outcome::ShutdownRequested;
            }
            Watched::Exited { status } => {
                let uptime_ms = started.elapsed().as_millis() as u64;
                if budget_resets(config, uptime_ms) {
                    restarts = 0;
                }
                if restarts >= config.max_restarts {
                    return Outcome::RestartBudgetExhausted;
                }
                let delay = backoff_ms(config, restarts);
                restarts += 1;
                log(&format!(
                    "the tenant exited ({status}); restart {restarts} of {} in {delay}ms",
                    config.max_restarts
                ));
                // Interruptible: a shutdown that arrived while the guest was waiting to restart is
                // a shutdown, not something to serve out the backoff first.
                if wait_for_signal(Duration::from_millis(u64::from(delay))) == Arrived::Shutdown {
                    return Outcome::ShutdownRequested;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watched {
    ShutdownRequested,
    Exited { status: i32 },
}

/// Polls the tenant's output and takes signals between reads.
///
/// The two together rather than one waiting on the other: a tenant printing steadily must not
/// delay a shutdown, and a tenant that has gone quiet must not stop this side noticing that its
/// log connection went away. The interval is what bounds both, and it is short enough that a
/// shutdown is not something anybody waits on.
fn watch(tenant: Pid, mut output: TenantOutput, forwarder: &mut Forwarder) -> Watched {
    loop {
        output.forward(forwarder);
        match wait_for_signal(POLL_INTERVAL) {
            Arrived::Shutdown => return Watched::ShutdownRequested,
            Arrived::ChildDied | Arrived::Nothing => {
                if let Some(status) = reap_until(tenant) {
                    // Whatever the tenant wrote before it went, before the pipes are dropped.
                    output.forward(forwarder);
                    return Watched::Exited { status };
                }
            }
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The read ends of the tenant's own stdout and stderr.
pub(crate) struct TenantOutput {
    stdout: std::fs::File,
    stderr: std::fs::File,
}

impl TenantOutput {
    /// Non-blocking, so a tenant that is quiet costs one syscall per stream and a tenant that is
    /// loud is drained until it is not.
    fn forward(&mut self, forwarder: &mut Forwarder) {
        for (stream, pipe) in [
            (protocol::TenantLogStream::Stdout, &mut self.stdout),
            (protocol::TenantLogStream::Stderr, &mut self.stderr),
        ] {
            let mut buffer = [0u8; 8192];
            loop {
                match std::io::Read::read(pipe, &mut buffer) {
                    Ok(0) => break,
                    Ok(read) => forwarder.write(stream, &buffer[..read]),
                    Err(_) => break,
                }
            }
        }
    }
}

pub(crate) struct Tenant {
    pid: Pid,
    output: TenantOutput,
}

/// The tenant's exit status if it was among the children that died, and `None` if it was not.
fn reap_until(tenant: Pid) -> Option<i32> {
    let mut tenant_status = None;
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => return tenant_status,
            Ok(status) => {
                let (pid, code) = match status {
                    WaitStatus::Exited(pid, code) => (pid, code),
                    WaitStatus::Signaled(pid, signal, _) => (pid, 128 + signal as i32),
                    _ => continue,
                };
                if pid == tenant {
                    tenant_status = Some(code);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arrived {
    Shutdown,
    ChildDied,
    Nothing,
}

/// `sigtimedwait` rather than a handler, because a signal taken deliberately at a point of this
/// loop's choosing cannot arrive between a fork and an exec. `nix` wraps the untimed form only.
#[allow(unsafe_code)]
fn wait_for_signal(within: Duration) -> Arrived {
    let blocked = waited_signals();
    // Built through `nix` rather than as a bare `libc::timespec`, whose `tv_sec` is a type that
    // differs between musl versions and is deprecated to name directly.
    let timeout = nix::sys::time::TimeSpec::from_duration(within);
    // Safety: the set is one this process built and has blocked, and the timeout is a struct this
    // frame owns for the length of the call. No info is asked for, so the second argument is null.
    let taken = unsafe { libc::sigtimedwait(blocked.as_ref(), std::ptr::null_mut(), timeout.as_ref()) };
    match Signal::try_from(taken) {
        Ok(Signal::SIGINT | Signal::SIGTERM) => Arrived::Shutdown,
        Ok(Signal::SIGCHLD) => Arrived::ChildDied,
        // A timeout, or an interruption: either way there is nothing to act on and the caller
        // decides whether to wait again.
        _ => Arrived::Nothing,
    }
}

/// SIGTERM, then SIGKILL once the grace period is up.
fn stop(tenant: Pid) {
    let _ = nix::sys::signal::kill(tenant, Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_millis(u64::from(SHUTDOWN_GRACE_MS));
    while Instant::now() < deadline {
        if reap_until(tenant).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    log("the tenant did not stop in time and was killed");
    let _ = nix::sys::signal::kill(tenant, Signal::SIGKILL);
    let _ = waitpid(tenant, None);
}

/// Forked and exec'd directly rather than through `std::process`, because PID 1 owns the reaping
/// of every child in the guest and a second reaper racing this one would lose exit statuses.
fn spawn(config: &InstanceConfig) -> Option<Tenant> {
    let executable = CString::new(paths::TENANT_BINARY).ok()?;
    let mut argv = vec![executable.clone()];
    for argument in &config.arguments {
        argv.push(CString::new(argument.as_str()).ok()?);
    }
    let environment: Vec<CString> = config
        .tenant_environment()
        .into_iter()
        .filter_map(|(name, value)| CString::new(format!("{name}={value}")).ok())
        .collect();

    // The tenant's own descriptors, so its output reaches the host rather than PID 1's console.
    // Non-blocking on this side: a tenant that fills a pipe nobody is draining should block, which
    // is back-pressure, but this side must never block reading one that is empty.
    let (stdout_read, stdout_write) = nix::unistd::pipe().ok()?;
    let (stderr_read, stderr_write) = nix::unistd::pipe().ok()?;
    for read in [&stdout_read, &stderr_read] {
        let _ = nix::fcntl::fcntl(read, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK));
    }

    // Safety: between the fork and the exec this calls only async-signal-safe functions, which is
    // the whole of what a forked child of a single-threaded process may do.
    match unsafe { nix::unistd::fork() } {
        Err(error) => {
            log(&format!("the tenant could not be forked: {error}"));
            None
        }
        Ok(ForkResult::Parent { child }) => {
            drop(stdout_write);
            drop(stderr_write);
            Some(Tenant {
                pid: child,
                output: TenantOutput {
                    stdout: std::fs::File::from(stdout_read),
                    stderr: std::fs::File::from(stderr_read),
                },
            })
        }
        Ok(ForkResult::Child) => {
            let failed = |_| unsafe { libc::_exit(127) };
            // Unblocked in the child, so a tenant that installs its own handlers gets the signals
            // its own users send it rather than a mask it never asked for.
            let _ = SigSet::all().thread_unblock();
            let _ = nix::unistd::dup2_stdout(&stdout_write).map_err(failed);
            let _ = nix::unistd::dup2_stderr(&stderr_write).map_err(failed);
            let _ = nix::unistd::chdir(paths::APP_DIR).map_err(failed);
            // The group first: after setuid there is no privilege left to change it with.
            let _ = nix::unistd::setgid(Gid::from_raw(paths::TENANT_GID)).map_err(failed);
            let _ = nix::unistd::setgroups(&[]).map_err(failed);
            let _ = nix::unistd::setuid(Uid::from_raw(paths::TENANT_UID)).map_err(failed);
            let _ = nix::unistd::execve(&executable, &argv, &environment);
            unsafe { libc::_exit(127) }
        }
    }
}
