//! Holding the tenant's filesystem still while the host reads the block device underneath it.
//!
//! Ported from `apps/runtime/src/guest-control.c`. The host cannot do this from its own side: the
//! guest holds the same filesystem mounted read-write, and what has only reached ext4's journal is
//! not yet in the place the host reads. Only the kernel that owns the mount can put it there.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use guest_contract::paths;

use crate::guest::{log, vsock};

const FREEZE_REQUEST: &str = "FREEZE";
const FREEZE_HELD: &str = "OK";

/// How long a freeze may be held before this side takes it back.
///
/// A tenant frozen indefinitely because a host went away mid-export is a tenant nothing can reach
/// and nothing will thaw. Long enough that an honest cut is never interrupted — the host's own
/// freeze window is the length of one `checkpoint create` — and short enough that a lost host
/// costs minutes rather than the life of the microVM.
const MAX_HOLD: Duration = Duration::from_secs(900);

const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn serve() -> ! {
    let Ok(listener) = vsock::listener(guest_contract::vsock::GUEST_CONTROL_VSOCK_PORT) else {
        log("the control port could not be opened; no export can freeze this tenant");
        // Not fatal, and not a busy loop either: the tenant still runs, and the failure surfaces
        // on the host as an export that could not freeze.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    };
    loop {
        match vsock::accept_one(&listener) {
            Ok(connection) => answer(connection),
            Err(_) => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Answers one connection and returns with the filesystem thawed, however that connection ended.
///
/// There is no path out of here that leaves a tenant frozen, which is the property worth having:
/// the freeze is superblock state that outlives the process holding it, so a worker that exited
/// without thawing would wedge a tenant until the microVM went.
fn answer(connection: OwnedFd) {
    let mut wire = BufReader::new(std::fs::File::from(connection));
    let mut request = String::new();
    if wire.read_line(&mut request).is_err() || request.trim() != FREEZE_REQUEST {
        return;
    }
    if let Err(error) = freeze(paths::DATA_DIR) {
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
    // The connection *is* the lease: the host holding it open is what says the freeze is still
    // wanted, and its going away — deliberately or not, which this side cannot tell apart and has
    // no reason to — is what ends it.
    let deadline = Instant::now() + MAX_HOLD;
    let mut byte = [0u8; 1];
    loop {
        match read_would_block(&mut wire, &mut byte) {
            Held::Gone => break,
            Held::Still if Instant::now() >= deadline => {
                log("a freeze was held past its ceiling and has been taken back");
                break;
            }
            Held::Still => std::thread::sleep(POLL_INTERVAL),
        }
    }
    thaw_quietly();
}

enum Held {
    Still,
    Gone,
}

fn read_would_block(wire: &mut BufReader<std::fs::File>, byte: &mut [u8; 1]) -> Held {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd {
        fd: wire.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // Safety: one descriptor this process owns, polled for the length of one interval.
    let ready = unsafe { libc::poll(&raw mut poll, 1, 0) };
    if ready <= 0 {
        return Held::Still;
    }
    // Readable means the far end sent something or hung up. Nothing is ever sent on this
    // connection after the reply, so either way the lease is over.
    match std::io::Read::read(wire.get_mut(), byte) {
        Ok(0) | Err(_) => Held::Gone,
        Ok(_) => Held::Gone,
    }
}

/// ext4 answers a freeze by checkpointing its journal, which is the whole point: it moves every
/// committed change out of a log the host never replays and into the blocks the host does read.
pub(crate) fn freeze(mount_point: &str) -> std::io::Result<()> {
    ioctl(mount_point, FIFREEZE)
}

/// `EINVAL` is a filesystem that was never frozen, which is not something to report.
pub(crate) fn thaw_quietly() {
    let _ = ioctl(paths::DATA_DIR, FITHAW);
}

const FIFREEZE: libc::c_ulong = 0xC0045877;
const FITHAW: libc::c_ulong = 0xC0045878;

#[allow(unsafe_code)]
fn ioctl(mount_point: &str, request: libc::c_ulong) -> std::io::Result<()> {
    let directory = std::fs::File::open(mount_point)?;
    let mut level: libc::c_int = 0;
    // Safety: the descriptor is a directory on the filesystem being frozen, and both requests take
    // a pointer to an int the kernel writes nothing meaningful into. There is no safe wrapper for
    // either in `nix`.
    let answered = unsafe { libc::ioctl(directory.as_raw_fd(), request as libc::Ioctl, &raw mut level) };
    if answered < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
