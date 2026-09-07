use std::os::fd::{AsRawFd, OwnedFd};

use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr};

const BACKLOG: usize = 4;

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
    Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(connection) })
}
