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

const TENANT_UMASK: libc::mode_t = 0o022;

pub(crate) fn run() -> ExitCode {
    if mounts::dev().is_err() {
        return shutdown(None);
    }
    adopt_console();
    log("guest runtime starting");

    supervisor::block_signals();
    route_ctrl_alt_del_here();

    let config = match boot() {
        Ok(config) => config,
        Err(reason) => {
            log(&reason);
            return shutdown(None);
        }
    };

    let channels = channels::start();

    match config.artifact_kind {
        protocol::ArtifactKind::Executable => log(&format!(
            "starting the tenant as uid {} with data at {}",
            paths::TENANT_UID,
            paths::DATA_DIR
        )),
        protocol::ArtifactKind::Rootfs => log(&format!(
            "starting the system's {} in namespaces of its own",
            paths::ROOTFS_INIT
        )),
    }
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
    match config.artifact_kind {
        protocol::ArtifactKind::Executable => prepare_tenant_filesystem()?,
        protocol::ArtifactKind::Rootfs => prepare_system_filesystem(&config)?,
    }
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
    mounts::tmpfs(paths::APP_DIR, "mode=0755,size=1M").map_err(|error| error.to_string())?;
    mounts::tenant_data(
        paths::DATA_DEVICE,
        paths::DATA_DIR,
        Some((paths::TENANT_UID, paths::TENANT_GID)),
    )
    .map_err(|error| error.to_string())
}

/// The system's root is the uploaded image with the volume's `upper` written over it. The volume
/// stays root's: what the system writes lands there as root wrote it. The DNS this host resolves
/// with is written into the stacked root rather than bind-mounted, since a stock image's
/// `/etc/resolv.conf` is as likely a dangling symlink as a file.
fn prepare_system_filesystem(config: &InstanceConfig) -> Result<(), String> {
    mounts::rootfs_image(paths::ARTIFACT_DEVICE, paths::ARTIFACT_MOUNT).map_err(|error| error.to_string())?;
    let init = std::path::Path::new(paths::ARTIFACT_MOUNT).join(paths::ROOTFS_INIT.trim_start_matches('/'));
    if std::fs::symlink_metadata(&init).is_err() {
        return Err(format!(
            "the root filesystem holds nothing at {}, so there is no system to start",
            paths::ROOTFS_INIT
        ));
    }
    mounts::tmpfs(paths::APP_DIR, "mode=0755,size=1M").map_err(|error| error.to_string())?;
    mounts::tenant_data(paths::DATA_DEVICE, paths::DATA_DIR, None).map_err(|error| error.to_string())?;
    let upper = format!("{}/{}", paths::DATA_DIR, paths::OVERLAY_UPPER_DIR);
    let work = format!("{}/{}", paths::DATA_DIR, paths::OVERLAY_WORK_DIR);
    mounts::overlay(paths::ARTIFACT_MOUNT, &upper, &work, paths::ROOTFS_MOUNT)
        .map_err(|error| error.to_string())?;

    let resolv_conf = std::path::Path::new(paths::ROOTFS_MOUNT).join("etc/resolv.conf");
    if std::fs::symlink_metadata(&resolv_conf).is_ok() {
        std::fs::remove_file(&resolv_conf)
            .map_err(|error| format!("{} could not be replaced: {error}", resolv_conf.display()))?;
    }
    if let Some(parent) = resolv_conf.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let rendered: String = config
        .nameservers
        .iter()
        .map(|address| format!("nameserver {address}\n"))
        .collect();
    std::fs::write(&resolv_conf, rendered)
        .map_err(|error| format!("{} could not be written: {error}", resolv_conf.display()))?;
    log(&format!(
        "system root stacked at {} over {}",
        paths::ROOTFS_MOUNT,
        paths::ARTIFACT_MOUNT
    ));
    Ok(())
}

fn shutdown(channels: Option<&channels::Channels>) -> ExitCode {
    if let Some(channels) = channels {
        channels.stop();
    }
    // A stacked root holds the volume open under it, so it comes down first or the volume never does.
    let _ = nix::mount::umount2(paths::ROOTFS_MOUNT, nix::mount::MntFlags::MNT_DETACH);
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

fn route_ctrl_alt_del_here() {
    if unsafe { libc::reboot(libc::RB_DISABLE_CAD) } < 0 {
        log("could not take over ctrl-alt-del");
    }
}

pub(crate) fn log(message: &str) {
    use std::io::Write;
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "[nibrun] {message}");
    let _ = stderr.flush();
}
