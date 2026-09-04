//! The Linux half: everything that only means anything as PID 1 inside a microVM.

mod channels;
mod control;
mod filesystem;
mod logs;
mod mounts;
mod supervisor;
mod vsock;

use std::process::ExitCode;

use guest_contract::instance_env::{parse_instance_env, InstanceConfig, CONFIG_MAX_BYTES};
use guest_contract::paths;

/// What the tenant's own files are created with, and what its umask is set to before anything of
/// its own runs: group and other may read, neither may write.
const TENANT_UMASK: libc::mode_t = 0o022;

pub(crate) fn run() -> ExitCode {
    // Before anything else, because until devtmpfs is mounted there is no `/dev/console` for the
    // kernel or for this to report anything on — including the failure to mount it.
    if mounts::dev().is_err() {
        return shutdown(None);
    }
    adopt_console();
    log("guest runtime starting");

    // Early enough that a shutdown arriving during boot is still honoured: PID 1 discards signals
    // it has neither blocked nor handled, so an unblocked SIGINT before this point is gone for
    // good.
    supervisor::block_signals();
    route_ctrl_alt_del_here();

    let config = match boot() {
        Ok(config) => config,
        Err(reason) => {
            log(&reason);
            return shutdown(None);
        }
    };

    // After the data filesystem exists and before the tenant does: both channels have that
    // filesystem as their only job, and the host may ask about it while the tenant is still
    // starting.
    let channels = channels::start();

    log(&format!(
        "starting the tenant as uid {} with data at {}",
        paths::TENANT_UID,
        paths::DATA_DIR
    ));
    match supervisor::supervise(&config) {
        supervisor::Ended::ShutdownRequested => log("the tenant has stopped; shutting the guest down"),
        supervisor::Ended::RestartBudgetExhausted => log(&format!(
            "the tenant used its {} restarts without staying up; shutting the guest down",
            config.max_restarts
        )),
        supervisor::Ended::SpawnFailed => {
            log("the tenant could not be started at all; shutting the guest down")
        }
    }
    shutdown(Some(&channels))
}

fn boot() -> Result<InstanceConfig, String> {
    unsafe { libc::umask(TENANT_UMASK) };
    mounts::pseudo_filesystems().map_err(|error| error.to_string())?;
    let config = read_instance_config()?;
    write_resolv_conf(&config)?;
    prepare_tenant_filesystem()?;
    Ok(config)
}

fn read_instance_config() -> Result<InstanceConfig, String> {
    mounts::config(paths::CONFIG_DEVICE, paths::CONFIG_MOUNT).map_err(|error| error.to_string())?;
    let text = std::fs::read_to_string(paths::CONFIG_FILE)
        .map_err(|error| format!("{} could not be read: {error}", paths::CONFIG_FILE))?;
    if text.len() > CONFIG_MAX_BYTES {
        return Err(format!(
            "{} is larger than this runtime reads",
            paths::CONFIG_FILE
        ));
    }
    let config = parse_instance_env(&text).map_err(|error| error.to_string())?;
    // The config drive carries the tenant's secrets; nothing needs it after this.
    nix::mount::umount(paths::CONFIG_MOUNT)
        .map_err(|error| format!("{} could not be unmounted: {error}", paths::CONFIG_MOUNT))?;
    log(&format!(
        "instance configured: port {}, {} environment variables, {} nameservers",
        config.http_port,
        config.environment.len(),
        config.nameservers.len()
    ));
    Ok(config)
}

fn write_resolv_conf(config: &InstanceConfig) -> Result<(), String> {
    if config.nameservers.is_empty() {
        log("instance.env names no DNS server, so the tenant will not resolve hostnames");
    }
    let rendered: String = config
        .nameservers
        .iter()
        .map(|address| format!("nameserver {address}\n"))
        .collect();
    std::fs::write(paths::RESOLV_CONF, rendered)
        .map_err(|error| format!("{} could not be written: {error}", paths::RESOLV_CONF))
}

fn prepare_tenant_filesystem() -> Result<(), String> {
    mounts::artifact(paths::ARTIFACT_DEVICE, paths::ARTIFACT_MOUNT).map_err(|error| error.to_string())?;
    // Checked here rather than left to execve, which would spend the whole restart budget on the
    // same EACCES. The tenant runs as an unprivileged uid, so a binary the artifact builder wrote
    // without world-execute cannot be started at all.
    let details = std::fs::metadata(paths::TENANT_BINARY)
        .map_err(|_| format!("the artifact drive holds no binary at {}", paths::TENANT_BINARY))?;
    if !details.is_file() {
        return Err(format!("{} is not a file", paths::TENANT_BINARY));
    }
    if std::os::unix::fs::PermissionsExt::mode(&details.permissions()) & 0o001 == 0 {
        return Err(format!(
            "{} is not executable by the uid it runs as",
            paths::TENANT_BINARY
        ));
    }
    // The tenant's working directory is a tmpfs it does not own: the only path it can write is the
    // data filesystem mounted inside it.
    mounts::tmpfs(paths::APP_DIR, "mode=0755,size=1M").map_err(|error| error.to_string())?;
    std::fs::create_dir_all(paths::DATA_DIR)
        .map_err(|error| format!("{} could not be made: {error}", paths::DATA_DIR))?;
    mounts::tenant_data(
        paths::DATA_DEVICE,
        paths::DATA_DIR,
        paths::TENANT_UID,
        paths::TENANT_GID,
    )
    .map_err(|error| error.to_string())
}

/// A guest reset is what Firecracker turns into "the microVM exited". Powering off or halting
/// instead leaves the VMM process running with nobody inside it, and the host would never see the
/// instance stop.
fn shutdown(channels: Option<&channels::Channels>) -> ExitCode {
    // Before the unmount: a filesystem left frozen would never return from one, and a worker still
    // holding a file open would keep it busy.
    if let Some(channels) = channels {
        channels.stop();
    }
    // Unmounted rather than only synced, so the next boot finds a clean filesystem instead of
    // replaying a journal.
    if let Err(error) = nix::mount::umount(paths::DATA_DIR) {
        if !matches!(error, nix::errno::Errno::EINVAL | nix::errno::Errno::ENOENT) {
            log(&format!("could not unmount {}: {error}", paths::DATA_DIR));
        }
    }
    nix::unistd::sync();
    unsafe { libc::reboot(libc::RB_AUTOBOOT) };
    log("the guest could not be shut down");
    loop {
        unsafe { libc::pause() };
    }
}

/// The rootfs image carries no device nodes of its own, so whether init starts with a stdin,
/// stdout and stderr at all depends on the kernel having mounted devtmpfs before executing it.
/// Claiming the console here gives runtime diagnostics a reliable sink; tenant output gets its own
/// descriptors when it is spawned.
fn adopt_console() {
    use std::os::fd::AsRawFd;
    let Ok(console) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/console")
    else {
        return;
    };
    let raw = console.as_raw_fd();
    for target in 0..=2 {
        if raw != target {
            unsafe { libc::dup2(raw, target) };
        }
    }
}

/// Firecracker's `SendCtrlAltDel` arrives as a key sequence, and the kernel's default response is
/// to reset the machine on the spot — which ends the VM with the tenant mid-write. Disabling it
/// turns the same sequence into a SIGINT to PID 1, which is the only way the guest ever hears
/// "please stop".
fn route_ctrl_alt_del_here() {
    if unsafe { libc::reboot(libc::RB_DISABLE_CAD) } < 0 {
        log("could not take over ctrl-alt-del");
    }
}

/// The console, directly. Nothing here buffers: a guest that died with its last line in a buffer
/// is a guest that did not say why.
pub(crate) fn log(message: &str) {
    use std::io::Write;
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "[nibrun] {message}");
    let _ = stderr.flush();
}
