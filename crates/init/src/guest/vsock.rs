//! Listening on a guest vsock port.
//!
//! The host reaches these through Firecracker's own multiplexer, which is why the numbers in
//! `guest-contract` are the whole of the agreement: nothing here has to know how the host got here.

use std::os::fd::{AsRawFd, OwnedFd};

use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr};

/// One waiter at a time is plenty: the host opens a connection, asks what it came to ask, and
/// closes it. A queue here would only hold requests the single worker behind it cannot start on.
const BACKLOG: usize = 4;

/// `VMADDR_CID_ANY`, because a guest binds without knowing its own context id.
const CID_ANY: u32 = 0xFFFF_FFFF;

pub(crate) fn listener(port: u32) -> nix::Result<OwnedFd> {
    let socket = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    bind(socket.as_raw_fd(), &VsockAddr::new(CID_ANY, port))?;
    listen(&socket, Backlog::new(BACKLOG as i32)?)?;
    Ok(socket)
}

pub(crate) fn accept_one(listener: &OwnedFd) -> nix::Result<OwnedFd> {
    let connection = accept(listener.as_raw_fd())?;
    // Safety: `accept` returns a descriptor this process owns and nothing else holds.
    Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(connection) })
}
