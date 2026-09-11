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

fn watch(tenant: Pid, mut output: TenantOutput, forwarder: &mut Forwarder) -> Watched {
    loop {
        output.forward(forwarder);
        match wait_for_signal(POLL_INTERVAL) {
            Arrived::Shutdown => return Watched::ShutdownRequested,
            Arrived::ChildDied | Arrived::Nothing => {
                if let Some(status) = reap_until(tenant) {
                    output.forward(forwarder);
                    return Watched::Exited { status };
                }
            }
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) struct TenantOutput {
    stdout: std::fs::File,
    stderr: std::fs::File,
}

impl TenantOutput {
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

#[allow(unsafe_code)]
fn wait_for_signal(within: Duration) -> Arrived {
    let blocked = waited_signals();
    let timeout = nix::sys::time::TimeSpec::from_duration(within);
    let taken = unsafe { libc::sigtimedwait(blocked.as_ref(), std::ptr::null_mut(), timeout.as_ref()) };
    match Signal::try_from(taken) {
        Ok(Signal::SIGINT | Signal::SIGTERM) => Arrived::Shutdown,
        Ok(Signal::SIGCHLD) => Arrived::ChildDied,
        _ => Arrived::Nothing,
    }
}

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

fn spawn(config: &InstanceConfig) -> Option<Tenant> {
    let root = CString::new(paths::ROOT_MOUNT).ok()?;
    let working_directory = CString::new(config.working_directory.as_str()).ok()?;
    let executable = CString::new(config.program.as_str()).ok()?;
    let mut argv = vec![executable.clone()];
    for argument in &config.arguments {
        argv.push(CString::new(argument.as_str()).ok()?);
    }
    let environment: Vec<CString> = config
        .tenant_environment()
        .into_iter()
        .filter_map(|(name, value)| CString::new(format!("{name}={value}")).ok())
        .collect();

    let (stdout_read, stdout_write) = nix::unistd::pipe().ok()?;
    let (stderr_read, stderr_write) = nix::unistd::pipe().ok()?;
    for read in [&stdout_read, &stderr_read] {
        let _ = nix::fcntl::fcntl(read, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK));
    }

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
            let _ = SigSet::all().thread_unblock();
            let _ = nix::unistd::dup2_stdout(&stdout_write).map_err(failed);
            let _ = nix::unistd::dup2_stderr(&stderr_write).map_err(failed);
            // Into the stacked root before the privileges go: chroot is root's to call, and a
            // process that is 65534 in a root it cannot leave is the whole of the isolation.
            let _ = nix::unistd::chroot(root.as_c_str()).map_err(failed);
            let _ = nix::unistd::chdir(working_directory.as_c_str()).map_err(failed);
            let _ = nix::unistd::setgid(Gid::from_raw(paths::TENANT_GID)).map_err(failed);
            let _ = nix::unistd::setgroups(&[]).map_err(failed);
            let _ = nix::unistd::setuid(Uid::from_raw(paths::TENANT_UID)).map_err(failed);
            let _ = nix::unistd::execve(&executable, &argv, &environment);
            unsafe { libc::_exit(127) }
        }
    }
}
