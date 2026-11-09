use std::io::{BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use guest_contract::paths;

use crate::guest::{log, vsock};

const FREEZE_REQUEST: &str = "FREEZE";
const FREEZE_HELD: &str = "OK";
const TENANT_FREEZE: &str = "/sys/fs/cgroup/tenant/cgroup.freeze";
const TENANT_EVENTS: &str = "/sys/fs/cgroup/tenant/cgroup.events";

const MAX_HOLD: Duration = Duration::from_secs(900);

// kvmclock is restored without advancing, so time spent paused cannot expire this lease.
const TENANT_LEASE: Duration = Duration::from_millis(guest_contract::control::TENANT_FREEZE_LEASE_MS);
const CONTROL_TIMEOUT: Duration = Duration::from_millis(guest_contract::control::TENANT_CONTROL_TIMEOUT_MS);

const ERROR_BACKOFF: Duration = Duration::from_millis(100);

pub(crate) fn serve() -> ! {
    let Ok(listener) = vsock::listener(guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT) else {
        log("the control port could not be opened; no export can freeze this tenant");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    };
    if nix::fcntl::fcntl(
        &listener,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .is_err()
    {
        log("the control listener could not be made nonblocking");
        loop {
            std::thread::sleep(ERROR_BACKOFF);
        }
    }
    let mut frozen_until = None;
    loop {
        if frozen_until.is_some_and(|deadline| Instant::now() >= deadline) {
            if std::fs::write(TENANT_FREEZE, "0").is_err() {
                std::thread::sleep(ERROR_BACKOFF);
                continue;
            }
            frozen_until = None;
            log("the host abandoned the tenant freeze; the tenant has been released");
        }
        match crate::control_wait::readable(&listener, frozen_until) {
            Ok(true) => match vsock::accept_from_host(&listener) {
                Ok(connection) => answer(connection, &mut frozen_until),
                Err(_) => std::thread::sleep(ERROR_BACKOFF),
            },
            Ok(false) => {}
            Err(_) => std::thread::sleep(ERROR_BACKOFF),
        }
    }
}

fn answer(connection: OwnedFd, frozen_until: &mut Option<Instant>) {
    use nix::sys::socket::{setsockopt, sockopt};
    use nix::sys::time::{TimeVal, TimeValLike};
    let timeout = TimeVal::milliseconds(CONTROL_TIMEOUT.as_millis() as i64);
    if setsockopt(&connection, sockopt::ReceiveTimeout, &timeout).is_err()
        || setsockopt(&connection, sockopt::SendTimeout, &timeout).is_err()
    {
        return;
    }
    let mut wire = BufReader::new(std::fs::File::from(connection));
    let Ok(request) = crate::clock_sync::request(&mut wire) else {
        return;
    };
    if request.trim() == guest_contract::control::TENANT_FREEZE_REQUEST {
        let result = crate::clock_sync::confirm_freeze(&mut wire).and_then(|()| freeze_tenant());
        if result.is_ok() {
            *frozen_until = Some(Instant::now() + TENANT_LEASE);
        }
        if wire
            .get_mut()
            .write_all(if result.is_ok() { b"OK\n" } else { b"REFUSED\n" })
            .is_err()
            && result.is_ok()
            && std::fs::write(TENANT_FREEZE, "0").is_ok()
        {
            *frozen_until = None;
        }
        return;
    }
    if request == guest_contract::control::TENANT_CLOCK_REQUEST {
        let result = synchronize_clock(&mut wire);
        if result.is_ok() {
            *frozen_until = None;
        }
        if let Err(error) = result {
            log(&format!("the tenant's clock could not be synchronized: {error}"));
            let _ = wire.get_mut().write_all(b"REFUSED\n");
        }
        return;
    }
    if request != FREEZE_REQUEST || frozen_until.is_some() {
        return;
    }
    if let Err(error) = freeze(paths::VOLUME_MOUNT) {
        log(&format!("the filesystem would not freeze: {error}"));
        let _ = wire.get_mut().write_all(b"REFUSED\n");
        return;
    }
    if wire
        .get_mut()
        .write_all(format!("{FREEZE_HELD}\n").as_bytes())
        .is_err()
    {
        thaw_quietly();
        return;
    }
    let deadline = Instant::now() + MAX_HOLD;
    if matches!(
        crate::control_wait::readable(wire.get_ref(), Some(deadline)),
        Ok(false)
    ) {
        log("a freeze was held past its ceiling and has been taken back");
    }
    thaw_quietly();
}

fn freeze_tenant() -> std::io::Result<()> {
    std::fs::write(TENANT_FREEZE, "1")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let events = match std::fs::read_to_string(TENANT_EVENTS) {
            Ok(events) => events,
            Err(error) => {
                let _ = std::fs::write(TENANT_FREEZE, "0");
                return Err(error);
            }
        };
        if events.lines().any(|line| line == "frozen 1") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            std::fs::write(TENANT_FREEZE, "0")?;
            return Err(std::io::Error::other("the tenant cgroup did not freeze"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn synchronize_clock(wire: &mut BufReader<std::fs::File>) -> std::io::Result<()> {
    let nanos = crate::clock_sync::host_time(wire)?;
    set_clock(nanos)?;
    std::fs::write(TENANT_FREEZE, "0")?;
    writeln!(
        wire.get_mut(),
        "{}",
        guest_contract::control::TENANT_CLOCK_RELEASED
    )
}

#[allow(unsafe_code)]
fn set_clock(nanos: u128) -> std::io::Result<()> {
    let seconds = i64::try_from(nanos / 1_000_000_000)
        .map_err(|_| std::io::Error::other("host time is out of range"))?;
    let time = libc::timespec {
        tv_sec: seconds,
        tv_nsec: (nanos % 1_000_000_000) as libc::c_long,
    };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &raw const time) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn freeze(mount_point: &str) -> std::io::Result<()> {
    ioctl(mount_point, FIFREEZE)
}

pub(crate) fn thaw_quietly() {
    let _ = ioctl(paths::VOLUME_MOUNT, FITHAW);
}

const FIFREEZE: libc::c_ulong = 0xC0045877;
const FITHAW: libc::c_ulong = 0xC0045878;

#[allow(unsafe_code)]
fn ioctl(mount_point: &str, request: libc::c_ulong) -> std::io::Result<()> {
    let directory = std::fs::File::open(mount_point)?;
    let mut level: libc::c_int = 0;
    let answered = unsafe { libc::ioctl(directory.as_raw_fd(), request as libc::Ioctl, &raw mut level) };
    if answered < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
