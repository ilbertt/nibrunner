use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use guest_contract::instance_env::InstanceConfig;
use guest_contract::paths;
use nix::sched::CloneFlags;
use nix::sys::signal::{SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{ForkResult, Gid, Pid, Uid};

use crate::guest::log;
use crate::guest::logs::Forwarder;
use crate::supervise::{
    backoff_ms, budget_resets, halt_for, stumbled_on, system_argv, system_environment, Halt, Outcome,
    Stumbled, SHUTDOWN_GRACE_MS,
};

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
        let Tenant {
            pid: tenant,
            output,
            halt,
        } = started_tenant;
        match watch(tenant, output, &mut forwarder) {
            Watched::ShutdownRequested => {
                stop(tenant, halt);
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
                match (halt, stumbled_on(status)) {
                    (Halt::PowerOff, Some(stumble)) => log(&format!(
                        "the system could not be entered: {stumble}; restart {restarts} of {} in {delay}ms",
                        config.max_restarts
                    )),
                    _ => log(&format!(
                        "the tenant exited ({status}); restart {restarts} of {} in {delay}ms",
                        config.max_restarts
                    )),
                }
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
    halt: Halt,
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

fn stop(tenant: Pid, halt: Halt) {
    let signal = match halt {
        Halt::Terminate => libc::SIGTERM,
        Halt::PowerOff => libc::SIGRTMIN() + 3,
    };
    let _ = unsafe { libc::kill(tenant.as_raw(), signal) };
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
    match config.artifact_kind {
        protocol::ArtifactKind::Executable => spawn_binary(config),
        protocol::ArtifactKind::Rootfs => spawn_system(config),
    }
}

fn pipes() -> Option<(TenantOutput, (OwnedFd, OwnedFd))> {
    let (stdout_read, stdout_write) = nix::unistd::pipe().ok()?;
    let (stderr_read, stderr_write) = nix::unistd::pipe().ok()?;
    for read in [&stdout_read, &stderr_read] {
        let _ = nix::fcntl::fcntl(read, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK));
    }
    Some((
        TenantOutput {
            stdout: std::fs::File::from(stdout_read),
            stderr: std::fs::File::from(stderr_read),
        },
        (stdout_write, stderr_write),
    ))
}

const SYSTEM_STACK_BYTES: usize = 256 * 1024;

/// The system's init, as PID 1 of namespaces of its own and a child of this one, which keeps
/// PID 1 here and with it the pipes, the restarts and the vsock channels.
///
/// The child is made with `clone` rather than `fork` so that it is born into its namespaces and
/// is this process's direct child: nothing relays its signals, and its death is its own. What
/// `clone` does not do is take the allocator's locks the way `fork` would, and this process has
/// threads holding them, so every string the child will need is built here and the child itself
/// makes only system calls.
fn spawn_system(config: &InstanceConfig) -> Option<Tenant> {
    let init = CString::new(paths::ROOTFS_INIT).ok()?;
    let argv: Vec<CString> = system_argv(config)
        .into_iter()
        .map(|argument| CString::new(argument).ok())
        .collect::<Option<_>>()?;
    let envp: Vec<CString> = system_environment(config)
        .into_iter()
        .map(|(name, value)| CString::new(format!("{name}={value}")).ok())
        .collect::<Option<_>>()?;
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|each| each.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|each| each.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());
    let root = CString::new(paths::ROOTFS_MOUNT).ok()?;
    let root_dev = CString::new(format!("{}/dev", paths::ROOTFS_MOUNT)).ok()?;
    let put_old = CString::new(format!("{}/mnt", paths::ROOTFS_MOUNT)).ok()?;
    let hostname: Option<String> = config.hostname.clone();

    let (output, (stdout_write, stderr_write)) = pipes()?;

    let entered = || -> isize {
        use nix::mount::{mount, umount2, MntFlags, MsFlags};
        let none: Option<&str> = None;
        let unprivileged = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
        let writable = MsFlags::MS_NOSUID | MsFlags::MS_NODEV;

        let _ = SigSet::all().thread_unblock();
        if nix::unistd::dup2_stdout(&stdout_write).is_err()
            || nix::unistd::dup2_stderr(&stderr_write).is_err()
        {
            return Stumbled::Pipes as isize;
        }
        if mount(none, "/", none, MsFlags::MS_REC | MsFlags::MS_PRIVATE, none).is_err() {
            return Stumbled::Propagation as isize;
        }
        if mount(
            Some("/dev"),
            root_dev.as_c_str(),
            none,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            none,
        )
        .is_err()
        {
            return Stumbled::Dev as isize;
        }
        if nix::unistd::pivot_root(root.as_c_str(), put_old.as_c_str()).is_err()
            || nix::unistd::chdir("/").is_err()
        {
            return Stumbled::Pivot as isize;
        }
        let _ = umount2("/mnt", MntFlags::MNT_DETACH);
        if mount(Some("proc"), "/proc", Some("proc"), unprivileged, none).is_err() {
            return Stumbled::Proc as isize;
        }
        if mount(Some("sysfs"), "/sys", Some("sysfs"), unprivileged, none).is_err() {
            return Stumbled::Sys as isize;
        }
        if mount(Some("tmpfs"), "/run", Some("tmpfs"), writable, Some("mode=0755")).is_err()
            || mount(Some("tmpfs"), "/tmp", Some("tmpfs"), writable, Some("mode=1777")).is_err()
        {
            return Stumbled::Scratch as isize;
        }
        // A kernel without cgroup2 is the system's to complain about, in its own words.
        let _ = mount(
            Some("cgroup2"),
            "/sys/fs/cgroup",
            Some("cgroup2"),
            unprivileged,
            none,
        );
        if let Some(hostname) = &hostname {
            let _ = nix::unistd::sethostname(hostname.as_str());
        }
        unsafe { libc::execve(init.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
        Stumbled::Exec as isize
    };

    let mut stack = vec![0u8; SYSTEM_STACK_BYTES];
    let flags = CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWCGROUP;
    let cloned = unsafe { nix::sched::clone(Box::new(entered), &mut stack, flags, Some(libc::SIGCHLD)) };
    match cloned {
        Ok(child) => Some(Tenant {
            pid: child,
            output,
            halt: Halt::PowerOff,
        }),
        Err(error) => {
            log(&format!(
                "the system could not be cloned into its namespaces: {error}"
            ));
            None
        }
    }
}

fn spawn_binary(config: &InstanceConfig) -> Option<Tenant> {
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

    let (output, (stdout_write, stderr_write)) = pipes()?;

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
                output,
                halt: halt_for(config.artifact_kind),
            })
        }
        Ok(ForkResult::Child) => {
            let failed = |_| unsafe { libc::_exit(127) };
            let _ = SigSet::all().thread_unblock();
            let _ = nix::unistd::dup2_stdout(&stdout_write).map_err(failed);
            let _ = nix::unistd::dup2_stderr(&stderr_write).map_err(failed);
            let _ = nix::unistd::chdir(paths::APP_DIR).map_err(failed);
            let _ = nix::unistd::setgid(Gid::from_raw(paths::TENANT_GID)).map_err(failed);
            let _ = nix::unistd::setgroups(&[]).map_err(failed);
            let _ = nix::unistd::setuid(Uid::from_raw(paths::TENANT_UID)).map_err(failed);
            let _ = nix::unistd::execve(&executable, &argv, &environment);
            unsafe { libc::_exit(127) }
        }
    }
}
