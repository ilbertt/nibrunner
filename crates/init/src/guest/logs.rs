//! Where the tenant's stdout and stderr go.
//!
//! Ported from `apps/runtime/src/guest-logs.c`. Guest-initiated, unlike the other two ports: the
//! host listens and this dials out, because a tenant's output starts the moment it does and there
//! is nothing to wait for a connection about.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use protocol::TenantLogStream;

/// The host's own context id, which is the only peer a guest vsock port has.
const CID_HOST: u32 = 2;

/// How long before a forwarder that could not reach the host tries again. A tenant printing into a
/// connection that is not there costs it nothing; retrying on every line would cost it a syscall
/// per line for as long as the host is away.
const RETRY_AFTER: Duration = Duration::from_secs(5);

pub(crate) struct Forwarder {
    connection: Option<std::fs::File>,
    retry_after: Instant,
    /// Counted rather than buffered. A tenant that outran the host is a tenant whose output is
    /// gone, and saying how much is the only honest thing left — a buffer would turn a slow reader
    /// into guest memory the tenant was not given.
    dropped_bytes: u64,
}

impl Forwarder {
    pub(crate) fn new() -> Self {
        Self {
            connection: None,
            retry_after: Instant::now(),
            dropped_bytes: 0,
        }
    }

    pub(crate) fn write(&mut self, stream: TenantLogStream, bytes: &[u8]) {
        if self.connection.is_none() {
            if Instant::now() < self.retry_after {
                self.dropped_bytes += bytes.len() as u64;
                return;
            }
            self.connection = dial().map(std::fs::File::from);
            self.retry_after = Instant::now() + RETRY_AFTER;
            self.declare_the_gap();
        }
        let Some(connection) = &mut self.connection else {
            self.dropped_bytes += bytes.len() as u64;
            return;
        };
        let frame = guest_contract::logs::encode_frame(guest_contract::logs::kind_of(stream), bytes);
        if connection.write_all(&frame).is_err() {
            // Dropped rather than retried on the spot: the host has gone, and the next write will
            // find that out again after the interval rather than on this line.
            self.connection = None;
            self.dropped_bytes += bytes.len() as u64;
        }
    }

    /// Said before anything else on a connection that has just come back.
    ///
    /// A reader who saw the output stop and start again cannot tell a quiet tenant from a lost
    /// one, and the count is the only thing this side still has: the bytes are gone, and holding
    /// them would have been guest memory the tenant was not given. Reset once it is said, so the
    /// same gap is never claimed twice.
    fn declare_the_gap(&mut self) {
        if self.dropped_bytes == 0 {
            return;
        }
        let Some(connection) = &mut self.connection else {
            return;
        };
        if connection
            .write_all(&guest_contract::logs::encode_gap(self.dropped_bytes))
            .is_ok()
        {
            self.dropped_bytes = 0;
        }
    }
}

fn dial() -> Option<OwnedFd> {
    let socket = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .ok()?;
    connect(
        socket.as_raw_fd(),
        &VsockAddr::new(CID_HOST, guest_contract::vsock::TENANT_LOG_VSOCK_PORT),
    )
    .ok()?;
    Some(socket)
}
