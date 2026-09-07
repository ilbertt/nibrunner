pub const TENANT_LOG_VSOCK_PORT: u32 = 51000;

pub const GUEST_CONTROL_VSOCK_PORT: u32 = 51001;

pub const GUEST_FILESYSTEM_VSOCK_PORT: u32 = 51002;

pub const GUEST_VSOCK_FILENAME: &str = "logs.vsock";

pub fn tenant_log_socket_name() -> String {
    format!("{GUEST_VSOCK_FILENAME}_{TENANT_LOG_VSOCK_PORT}")
}

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
        Err(GuestPortUnreachable {
            port,
            reply: reply.to_string(),
        })
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
