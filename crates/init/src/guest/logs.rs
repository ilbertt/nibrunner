use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use protocol::TenantLogStream;

const CID_HOST: u32 = 2;

const RETRY_AFTER: Duration = Duration::from_secs(5);

pub(crate) struct Forwarder {
    connection: Option<std::fs::File>,
    retry_after: Instant,
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
            self.connection = None;
            self.dropped_bytes += bytes.len() as u64;
        }
    }

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
