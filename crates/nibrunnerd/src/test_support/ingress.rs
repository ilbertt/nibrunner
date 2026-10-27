use std::fs::File;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::Duration;

const HOST_IP: Ipv4Addr = Ipv4Addr::new(198, 19, 0, 1);

pub struct IngressClient {
    namespace: String,
    interface: String,
}

fn command(program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .output()
        .expect("the networking tools are installed");
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl IngressClient {
    pub fn start() -> Self {
        let pid = std::process::id();
        let client = Self {
            namespace: format!("nibrunner-ingress-{pid}"),
            interface: format!("nbi{pid}"),
        };
        let namespace = &client.namespace;
        let interface = &client.interface;
        command("ip", &["netns", "add", namespace]);
        command(
            "ip",
            &[
                "link", "add", interface, "type", "veth", "peer", "name", "peer", "netns", namespace,
            ],
        );
        command("ip", &["address", "add", "198.19.0.1/30", "dev", interface]);
        command("ip", &["link", "set", interface, "up"]);
        command(
            "ip",
            &["-n", namespace, "address", "add", "198.19.0.2/30", "dev", "peer"],
        );
        command("ip", &["-n", namespace, "link", "set", "peer", "up"]);
        command("ip", &["-n", namespace, "link", "set", "lo", "up"]);
        command(
            "ip",
            &[
                "-n",
                namespace,
                "route",
                "add",
                "default",
                "via",
                &HOST_IP.to_string(),
            ],
        );
        // Docker's FORWARD policy must not be what makes an ingress rejection pass.
        for direction in ["-i", "-o"] {
            command(
                "iptables",
                &["-I", "FORWARD", direction, interface, "-j", "ACCEPT"],
            );
        }
        client
    }

    pub fn host_address(&self, port: u16) -> SocketAddr {
        SocketAddr::from((HOST_IP, port))
    }

    pub fn connect(&self, address: SocketAddr) -> std::io::Result<TcpStream> {
        let namespace = File::open(format!("/var/run/netns/{}", self.namespace))?;
        std::thread::spawn(move || {
            #[allow(
                unsafe_code,
                reason = "only this dedicated thread enters the caller's network namespace"
            )]
            let entered = unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) };
            if entered != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
            stream.set_read_timeout(Some(Duration::from_secs(30)))?;
            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
            Ok(stream)
        })
        .join()
        .expect("the ingress client thread finishes")
    }
}

impl Drop for IngressClient {
    fn drop(&mut self) {
        for direction in ["-i", "-o"] {
            let _ = Command::new("iptables")
                .args(["-D", "FORWARD", direction, &self.interface, "-j", "ACCEPT"])
                .output();
        }
        let _ = Command::new("ip")
            .args(["link", "delete", &self.interface])
            .output();
        let _ = Command::new("ip")
            .args(["netns", "delete", &self.namespace])
            .output();
    }
}
