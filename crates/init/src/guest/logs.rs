use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use guest_contract::logs::{encode_frame, encode_gap, encode_restart, kind_of, FRAME_HEADER_BYTES};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use protocol::{TenantLogStream, TenantRestart};

const CID_HOST: u32 = 2;

const RETRY_AFTER: Duration = Duration::from_secs(5);

type Dial = Box<dyn FnMut() -> Option<OwnedFd>>;

pub(crate) struct Forwarder {
    dial: Dial,
    connection: Option<std::fs::File>,
    retry_after: Instant,
    dropped_bytes: u64,
}

impl Forwarder {
    pub(crate) fn new() -> Self {
        Self::dialing(Box::new(dial))
    }

    pub(crate) fn dialing(dial: Dial) -> Self {
        Self {
            dial,
            connection: None,
            retry_after: Instant::now(),
            dropped_bytes: 0,
        }
    }

    pub(crate) fn write(&mut self, stream: TenantLogStream, bytes: &[u8]) {
        self.send(encode_frame(kind_of(stream), bytes));
    }

    /// Sent the way the tenant's output is, resend and all: a restart the host was not there to
    /// hear of is so many bytes in the next gap, and one fewer on its count.
    pub(crate) fn restarted(&mut self, restart: &TenantRestart) {
        self.send(encode_restart(restart));
    }

    fn send(&mut self, frame: Vec<u8>) {
        let delivered = if self.connection.is_some() {
            // A connection that fails is as likely one a snapshot captured, or one a host daemon
            // since restarted was holding, as a host gone away: the write is the first news of
            // it, so a fresh connection carries the same frame before anything is given up.
            self.deliver(&frame) || (self.redial() && self.deliver(&frame))
        } else {
            Instant::now() >= self.retry_after && self.redial() && self.deliver(&frame)
        };
        if !delivered {
            self.dropped_bytes += (frame.len() - FRAME_HEADER_BYTES) as u64;
        }
    }

    /// True when the host has the frame; false with the connection gone when it has not.
    fn deliver(&mut self, frame: &[u8]) -> bool {
        let Some(connection) = &mut self.connection else {
            return false;
        };
        if connection.write_all(frame).is_ok() {
            return true;
        }
        self.connection = None;
        false
    }

    /// A fresh connection, told first what it missed. Whether or not it comes, the next attempt
    /// at one waits `RETRY_AFTER`, so a host that is not listening is not asked on every line.
    fn redial(&mut self) -> bool {
        self.retry_after = Instant::now() + RETRY_AFTER;
        self.connection = (self.dial)().map(std::fs::File::from);
        if self.connection.is_none() {
            return false;
        }
        if self.dropped_bytes > 0 {
            if !self.deliver(&encode_gap(self.dropped_bytes)) {
                return false;
            }
            self.dropped_bytes = 0;
        }
        true
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Read;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::rc::Rc;

    use guest_contract::logs::{decode_frames, GuestLogFrame};
    use protocol::{StateMessage, TenantExit};

    use super::*;

    /// A host listening at `path`, counting how many times the forwarder asked for it.
    struct Host {
        listener: UnixListener,
        dials: Rc<Cell<u32>>,
    }

    impl Host {
        fn listening_at(path: &Path) -> Self {
            let listener = UnixListener::bind(path).unwrap();
            listener.set_nonblocking(true).unwrap();
            Self {
                listener,
                dials: Rc::new(Cell::new(0)),
            }
        }

        fn forwarder(&self, path: &Path) -> Forwarder {
            let path = path.to_path_buf();
            let dials = Rc::clone(&self.dials);
            Forwarder::dialing(Box::new(move || {
                dials.set(dials.get() + 1);
                UnixStream::connect(&path).ok().map(OwnedFd::from)
            }))
        }

        fn accept(&self) -> UnixStream {
            let (connection, _) = self.listener.accept().unwrap();
            connection.set_nonblocking(true).unwrap();
            connection
        }

        fn nothing_pending(&self) {
            assert!(self.listener.accept().is_err(), "a connection nobody dialed");
        }
    }

    fn arrived(connection: &mut UnixStream) -> Vec<GuestLogFrame> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match connection.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        let (frames, rest) = decode_frames(&[], &bytes).unwrap();
        assert!(rest.is_empty(), "a frame arrived in part");
        frames
    }

    /// The host's end of a connection, gone. Shut down and not only dropped: the supervisor's
    /// tests fork this process, and a child of theirs may hold a copy of the descriptor that
    /// would keep a dropped socket open.
    fn gone(connection: UnixStream) {
        connection.shutdown(std::net::Shutdown::Both).unwrap();
    }

    fn stdout(text: &str) -> GuestLogFrame {
        GuestLogFrame::Data {
            stream: TenantLogStream::Stdout,
            bytes: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_write_that_fails_once_is_resent_on_a_fresh_connection_and_nothing_is_declared_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.sock");
        let host = Host::listening_at(&path);
        let mut forwarder = host.forwarder(&path);

        forwarder.write(TenantLogStream::Stdout, b"before\n");
        let mut first = host.accept();
        assert_eq!(arrived(&mut first), vec![stdout("before\n")]);

        // The host daemon restarts: what it held is gone, and it listens afresh.
        gone(first);
        forwarder.write(TenantLogStream::Stdout, b"after\n");

        let mut second = host.accept();
        assert_eq!(arrived(&mut second), vec![stdout("after\n")]);
        assert_eq!(host.dials.get(), 2);
        assert_eq!(forwarder.dropped_bytes, 0);
    }

    fn restart() -> TenantRestart {
        TenantRestart {
            attempt: 1,
            budget: 5,
            exit: TenantExit::Signal(9),
            reason: StateMessage::new("the tenant exited (137); restart 1 of 5 in 500ms"),
            backoff_ms: 500,
        }
    }

    #[test]
    fn a_restart_takes_its_place_after_the_output_and_is_resent_like_it_on_a_fresh_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.sock");
        let host = Host::listening_at(&path);
        let mut forwarder = host.forwarder(&path);

        forwarder.write(TenantLogStream::Stderr, b"out of memory\n");
        forwarder.restarted(&restart());
        let mut first = host.accept();
        assert_eq!(
            arrived(&mut first),
            vec![
                GuestLogFrame::Data {
                    stream: TenantLogStream::Stderr,
                    bytes: b"out of memory\n".to_vec(),
                },
                GuestLogFrame::Restart(restart()),
            ]
        );

        gone(first);
        forwarder.restarted(&restart());
        let mut second = host.accept();
        assert_eq!(arrived(&mut second), vec![GuestLogFrame::Restart(restart())]);
        assert_eq!(forwarder.dropped_bytes, 0);
    }

    #[test]
    fn a_restart_nobody_heard_is_so_many_bytes_of_the_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.sock");
        let mut forwarder = Forwarder::dialing(Box::new(move || {
            UnixStream::connect(&path).ok().map(OwnedFd::from)
        }));
        forwarder.restarted(&restart());
        let payload_bytes = encode_restart(&restart()).len() - FRAME_HEADER_BYTES;
        assert_eq!(forwarder.dropped_bytes, payload_bytes as u64);
    }

    #[test]
    fn a_host_that_stays_down_accumulates_the_gap_and_hears_it_on_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.sock");
        let host = Host::listening_at(&path);
        let mut forwarder = host.forwarder(&path);
        forwarder.write(TenantLogStream::Stdout, b"before\n");
        let mut first = host.accept();
        assert_eq!(arrived(&mut first), vec![stdout("before\n")]);

        // Gone, and not back: the resend finds nobody listening.
        gone(first);
        drop(host);
        std::fs::remove_file(&path).unwrap();
        forwarder.write(TenantLogStream::Stdout, b"one\n");
        assert_eq!(forwarder.dropped_bytes, 4);
        forwarder.write(TenantLogStream::Stdout, b"two\n");
        assert_eq!(forwarder.dropped_bytes, 8);

        // Back, but the forwarder is not due to ask yet.
        let host = Host::listening_at(&path);
        forwarder.write(TenantLogStream::Stderr, b"three\n");
        assert_eq!(forwarder.dropped_bytes, 14);
        host.nothing_pending();

        forwarder.retry_after = Instant::now();
        forwarder.write(TenantLogStream::Stdout, b"four\n");
        let mut reconnected = host.accept();
        assert_eq!(
            arrived(&mut reconnected),
            vec![GuestLogFrame::Gap { dropped_bytes: 14 }, stdout("four\n")]
        );
        assert_eq!(forwarder.dropped_bytes, 0);
    }

    #[test]
    fn a_host_that_was_never_there_is_asked_once_and_then_left_alone_for_a_while() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.sock");
        let dials = Rc::new(Cell::new(0));
        let counted = Rc::clone(&dials);
        let mut forwarder = Forwarder::dialing(Box::new(move || {
            counted.set(counted.get() + 1);
            UnixStream::connect(&path).ok().map(OwnedFd::from)
        }));
        for _ in 0..10 {
            forwarder.write(TenantLogStream::Stdout, b"line\n");
        }
        assert_eq!(dials.get(), 1);
        assert_eq!(forwarder.dropped_bytes, 50);
    }
}
