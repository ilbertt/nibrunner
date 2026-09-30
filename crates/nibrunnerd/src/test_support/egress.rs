use std::fs::File;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

// A private address would be rejected independently of the configurable egress deny. This
// benchmarking subnet is routed only to the fixture's namespace and never to the internet.
const ENDPOINT_IP: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);

pub struct EgressEndpoint {
    namespace: String,
    interface: String,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    address: SocketAddr,
}

fn ip(args: &[&str]) {
    command("ip", args);
}

fn command(program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .output()
        .expect("the guest networking tools are installed");
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl EgressEndpoint {
    pub fn start() -> Self {
        let pid = std::process::id();
        let mut endpoint = Self {
            namespace: format!("nibrunner-egress-{pid}"),
            interface: format!("nbe{pid}"),
            stop: Arc::new(AtomicBool::new(false)),
            listener: None,
            address: SocketAddr::from((ENDPOINT_IP, 0)),
        };
        let namespace = &endpoint.namespace;
        let interface = &endpoint.interface;
        ip(&["netns", "add", namespace]);
        ip(&[
            "link", "add", interface, "type", "veth", "peer", "name", "peer", "netns", namespace,
        ]);
        ip(&["address", "add", "198.18.0.1/30", "dev", interface]);
        ip(&["link", "set", interface, "up"]);
        ip(&["-n", namespace, "address", "add", "198.18.0.2/30", "dev", "peer"]);
        ip(&["-n", namespace, "link", "set", "peer", "up"]);
        ip(&["-n", namespace, "link", "set", "lo", "up"]);

        // Docker sets the runner's FORWARD policy to DROP. Allow only this fixture's link
        // through that chain; the daemon's separate nftables chain still enforces its denies.
        command("iptables", &["-I", "FORWARD", "-o", interface, "-j", "ACCEPT"]);
        command("iptables", &["-I", "FORWARD", "-i", interface, "-j", "ACCEPT"]);

        let namespace = File::open(format!("/var/run/netns/{namespace}")).expect("the test namespace");
        let stop = endpoint.stop.clone();
        let (ready, listening) = std::sync::mpsc::channel();
        endpoint.listener = Some(std::thread::spawn(move || {
            // Only this dedicated listener thread enters the namespace; the host and guests
            // remain in the original one, so their packets exercise the forwarding hook.
            #[allow(
                unsafe_code,
                reason = "network namespace entry has no safe standard-library API"
            )]
            let entered = unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) };
            assert_eq!(entered, 0, "setns failed: {}", std::io::Error::last_os_error());
            let listener =
                TcpListener::bind((ENDPOINT_IP, 0)).expect("a listener outside the host namespace");
            listener.set_nonblocking(true).expect("a nonblocking listener");
            ready
                .send(listener.local_addr().expect("the listener address"))
                .expect("the test is waiting");
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("the egress listener failed: {error}"),
                }
            }
        }));
        endpoint.address = listening
            .recv_timeout(Duration::from_secs(5))
            .expect("the egress listener starts");
        endpoint
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn denied_cidr(&self) -> String {
        format!("{ENDPOINT_IP}/32")
    }
}

impl Drop for EgressEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
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
