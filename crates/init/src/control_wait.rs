use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

pub(crate) fn readable(socket: &impl AsFd, deadline: Option<Instant>) -> nix::Result<bool> {
    loop {
        let timeout = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(false);
                }
                let millis = remaining.as_nanos().div_ceil(Duration::from_millis(1).as_nanos());
                PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX)
            }
            None => PollTimeout::NONE,
        };
        let mut watched = [PollFd::new(socket.as_fd(), PollFlags::POLLIN)];
        match poll(&mut watched, timeout) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_)
                if watched[0]
                    .revents()
                    .is_some_and(|events| events.contains(PollFlags::POLLNVAL)) =>
            {
                return Err(Errno::EBADF);
            }
            Ok(_) => return Ok(true),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{mpsc, Arc, Barrier};

    use super::*;

    #[test]
    fn a_connection_interrupts_the_wait_before_the_lease_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let waiting = barrier.clone();
        let (answer, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            waiting.wait();
            answer
                .send(readable(&listener, Some(Instant::now() + Duration::from_secs(2))))
                .unwrap();
        });
        barrier.wait();
        let _connection = UnixStream::connect(path).unwrap();
        assert!(received.recv_timeout(Duration::from_secs(1)).unwrap().unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn an_abandoned_lease_expires_without_a_new_connection() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_millis(20);
        assert!(!readable(&socket, Some(deadline)).unwrap());
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn an_expired_lease_does_not_wait_on_an_idle_socket() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        assert!(!readable(&socket, Some(Instant::now())).unwrap());
    }

    #[test]
    fn a_disconnect_wakes_an_indefinite_wait() {
        let (socket, peer) = UnixStream::pair().unwrap();
        drop(peer);
        assert!(readable(&socket, None).unwrap());
    }
}
