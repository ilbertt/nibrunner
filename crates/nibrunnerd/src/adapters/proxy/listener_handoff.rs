use std::io::{IoSlice, IoSliceMut};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};

const LISTENER: &[u8] = b"L";

pub(super) fn send(channel: &UnixStream, listener: &TcpListener) -> std::io::Result<()> {
    let descriptors = [listener.as_raw_fd()];
    let sent = sendmsg::<()>(
        channel.as_raw_fd(),
        &[IoSlice::new(LISTENER)],
        &[ControlMessage::ScmRights(&descriptors)],
        MsgFlags::empty(),
        None,
    )?;
    if sent != LISTENER.len() {
        return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
    }
    Ok(())
}

pub(super) fn receive(channel: &UnixStream) -> std::io::Result<TcpListener> {
    let mut tag = [0];
    let mut descriptors = Vec::new();
    let mut control = nix::cmsg_space!([RawFd; 16]);
    #[cfg(target_os = "linux")]
    let flags = MsgFlags::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = MsgFlags::empty();
    let (bytes, truncated) = {
        let mut buffers = [IoSliceMut::new(&mut tag)];
        let mut message = recvmsg::<()>(channel.as_raw_fd(), &mut buffers, Some(&mut control), flags)?;
        let truncated = message.flags.contains(MsgFlags::MSG_CTRUNC);
        // Even a rejected message owns the descriptors that fit in its control buffer.
        message.flags.remove(MsgFlags::MSG_CTRUNC);
        for control in message.cmsgs()? {
            if let ControlMessageOwned::ScmRights(received) = control {
                for descriptor in received {
                    #[allow(
                        unsafe_code,
                        reason = "SCM_RIGHTS transfers ownership of each received descriptor"
                    )]
                    descriptors.push(unsafe { OwnedFd::from_raw_fd(descriptor) });
                }
            }
        }
        (message.bytes, truncated)
    };
    if truncated || bytes != LISTENER.len() || tag != LISTENER || descriptors.len() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "listener handoff requires exactly one descriptor and its listener tag",
        ));
    }
    let descriptor = descriptors.pop().expect("one received descriptor was checked");
    nix::fcntl::fcntl(
        &descriptor,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )?;
    #[cfg(target_os = "linux")]
    if !nix::sys::socket::getsockopt(&descriptor, nix::sys::socket::sockopt::AcceptConn)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the handed-off descriptor is not a listening socket",
        ));
    }
    let listener = TcpListener::from(descriptor);
    listener
        .local_addr()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    #[test]
    #[ignore = "subprocess fixture; the handoff test invokes this with --ignored"]
    fn receiving_process() {
        let Some(path) = std::env::var_os("NIBRUNNER_TEST_LISTENER_HANDOFF") else {
            return;
        };
        let channel = UnixStream::connect(path).unwrap();
        let listener = receive(&channel).unwrap();
        assert!(nix::fcntl::FdFlag::from_bits_retain(
            nix::fcntl::fcntl(&listener, nix::fcntl::FcntlArg::F_GETFD).unwrap()
        )
        .contains(nix::fcntl::FdFlag::FD_CLOEXEC));
        listener.set_nonblocking(false).unwrap();
        let (mut request, _) = listener.accept().unwrap();
        request.write_all(b"accepted by replacement").unwrap();
    }

    #[tokio::test]
    async fn another_process_accepts_a_queued_connection_after_the_sender_closes_its_listener() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handoff.sock");
        let channel = tokio::net::UnixListener::bind(&path).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut queued = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        queued.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "adapters::proxy::listener_handoff::tests::receiving_process",
                "--ignored",
            ])
            .env("NIBRUNNER_TEST_LISTENER_HANDOFF", &path)
            .kill_on_drop(true);
        let child = child.spawn().unwrap();
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), channel.accept())
            .await
            .unwrap()
            .unwrap();
        send(&socket.into_std().unwrap(), &listener).unwrap();
        drop(listener);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(status.status.success());
        let mut response = String::new();
        queued.read_to_string(&mut response).unwrap();
        assert_eq!(response, "accepted by replacement");
    }

    #[test]
    fn a_message_without_a_listener_is_refused() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(LISTENER).unwrap();
        assert_eq!(
            receive(&receiver).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_connected_socket_cannot_be_handed_off_as_a_listener() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (connected, _) = UnixStream::pair().unwrap();
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[IoSlice::new(LISTENER)],
            &[ControlMessage::ScmRights(&[connected.as_raw_fd()])],
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        assert_eq!(
            receive(&receiver).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn unexpected_extra_descriptors_are_closed_when_the_message_is_refused() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[IoSlice::new(LISTENER)],
            &[ControlMessage::ScmRights(&[
                listener.as_raw_fd(),
                listener.as_raw_fd(),
            ])],
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        assert_eq!(
            receive(&receiver).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        drop(listener);
        assert!(std::net::TcpStream::connect(address).is_err());
    }
}
