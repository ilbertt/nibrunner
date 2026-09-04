//! One Firecracker vsock device serves the whole VM, so every port below multiplexes over it.
//! `apps/runtime/src/vsock.h` is the other end.

/// Tenant output goes out on this port, guest-initiated: Firecracker connects to
/// `<uds_path>_<port>` on the host side.
pub const TENANT_LOG_VSOCK_PORT: u32 = 51000;

/// Answered by the control process in the guest's PID 1, host-initiated.
pub const GUEST_CONTROL_VSOCK_PORT: u32 = 51001;

/// Answered by a second process beside it, on a port of its own because the control port
/// serialises: a granted freeze holds it for as long as an export reads the device.
pub const GUEST_FILESYSTEM_VSOCK_PORT: u32 = 51002;

/// The name of the vsock device in a microVM's working directory. The host dials a guest port by
/// connecting here and asking; the guest's own outbound connections arrive on `<path>_<port>`.
///
/// The value is what the tenant log port made it before anything else shared the device, and it
/// stays: a guest booted by an earlier daemon is listening on this path, and renaming it would
/// make a freeze of that VM find nothing, conclude no VMM was running, and read the volume
/// unfrozen.
pub const GUEST_VSOCK_FILENAME: &str = "logs.vsock";

pub fn tenant_log_socket_name() -> String {
    format!("{GUEST_VSOCK_FILENAME}_{TENANT_LOG_VSOCK_PORT}")
}

/// Firecracker's host-initiated leg is a text handshake before the stream becomes the guest's:
/// `CONNECT <port>` in, `OK <host-side port>` back. Anything else means the VMM is there and the
/// guest is not listening, which is a different thing from no VMM at all.
pub fn connect_request(port: u32) -> String {
    format!("CONNECT {port}\n")
}

const CONNECT_ACCEPTED: &str = "OK ";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("nothing in the guest answered vsock port {port}: {reply}")]
pub struct GuestPortUnreachable {
    pub port: u32,
    pub reply: String,
}

pub fn read_connect_reply(reply: &str, port: u32) -> Result<(), GuestPortUnreachable> {
    if reply.starts_with(CONNECT_ACCEPTED) {
        Ok(())
    } else {
        Err(GuestPortUnreachable { port, reply: reply.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_asks_for_the_control_port_by_number() {
        assert_eq!(connect_request(GUEST_CONTROL_VSOCK_PORT), "CONNECT 51001\n");
        assert_eq!(tenant_log_socket_name(), "logs.vsock_51000");
    }

    #[test]
    fn only_a_reply_naming_the_bound_port_is_accepted() {
        assert!(read_connect_reply("OK 1234", GUEST_CONTROL_VSOCK_PORT).is_ok());
        for reply in ["FAILED", "", "OK", "NOT OK 1234"] {
            assert!(read_connect_reply(reply, GUEST_CONTROL_VSOCK_PORT).is_err());
        }
    }
}
