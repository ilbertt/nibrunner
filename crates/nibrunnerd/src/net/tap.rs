//! The tap a slot names, and the host's own view of the guest behind it.
//!
//! Done in this process rather than by spawning `ip`: a tap is one ioctl on `/dev/net/tun` and
//! the rest is netlink, which is a smaller thing to depend on than iproute2 being installed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use protocol::Ipv4Address;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapInterface {
    pub tap_name: String,
    pub host_ipv4: Ipv4Address,
    pub subnet_prefix_length: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbour {
    pub guest_ipv4: Ipv4Address,
    pub guest_mac: String,
    pub tap_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{what} could not be done to {device}: {reason}")]
pub struct NetworkError {
    pub what: &'static str,
    pub device: String,
    pub reason: String,
}

impl NetworkError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

#[async_trait]
pub trait HostNetwork: Send + Sync {
    /// The tap, its address and its link state, brought to what a slot says they should be.
    /// Idempotent, so a converged host re-runs this to no effect.
    async fn ensure_tap(&self, tap: &TapInterface) -> Result<(), NetworkError>;

    /// The host's neighbour entry for a guest goes stale while its microVM is down, and the first
    /// connection after a wake then pays ARP re-resolution: 1.1s against the 112ms a refreshed
    /// entry costs, which is most of what makes waking cheaper than a cold boot. A guest keeps its
    /// address and its MAC across a sleep, so this writes back the pairing that was already there
    /// rather than announcing a new one.
    async fn refresh_neighbour(&self, neighbour: &Neighbour) -> Result<(), NetworkError>;

    async fn tap_names(&self) -> Vec<String>;
}

/// Records what it was asked for, so the order a boot does things in is a test rather than a host.
#[derive(Default)]
pub struct RecordingNetwork {
    taps: Mutex<Vec<TapInterface>>,
    neighbours: Mutex<Vec<Neighbour>>,
}

impl RecordingNetwork {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn taps(&self) -> Vec<TapInterface> {
        self.taps.lock().expect("no panic holds this lock").clone()
    }

    pub fn neighbours(&self) -> Vec<Neighbour> {
        self.neighbours.lock().expect("no panic holds this lock").clone()
    }
}

#[async_trait]
impl HostNetwork for RecordingNetwork {
    async fn ensure_tap(&self, tap: &TapInterface) -> Result<(), NetworkError> {
        self.taps.lock().expect("no panic holds this lock").push(tap.clone());
        Ok(())
    }

    async fn refresh_neighbour(&self, neighbour: &Neighbour) -> Result<(), NetworkError> {
        self.neighbours.lock().expect("no panic holds this lock").push(neighbour.clone());
        Ok(())
    }

    async fn tap_names(&self) -> Vec<String> {
        self.taps().into_iter().map(|tap| tap.tap_name).collect()
    }
}

#[cfg(target_os = "linux")]
pub use linux::KernelNetwork;

#[cfg(target_os = "linux")]
mod linux {
    use std::net::IpAddr;

    use async_trait::async_trait;
    use futures::TryStreamExt;
    use netlink_packet_route::link::LinkAttribute;
    use netlink_packet_route::neighbour::NeighbourState;
    use rtnetlink::{Handle, LinkTun, LinkUnspec};

    use super::{HostNetwork, Neighbour, NetworkError, TapInterface};

    pub struct KernelNetwork {
        handle: Handle,
    }

    impl KernelNetwork {
        /// The connection is a task of its own for as long as the daemon runs: every request goes
        /// through it, and dropping it would leave every later call answering nothing.
        pub fn open() -> Result<Self, NetworkError> {
            let (connection, handle, _) = rtnetlink::new_connection().map_err(|error| NetworkError {
                what: "a netlink socket",
                device: "netlink".into(),
                reason: error.to_string(),
            })?;
            tokio::spawn(connection);
            Ok(Self { handle })
        }

        async fn index_of(&self, tap_name: &str) -> Option<u32> {
            self.handle
                .link()
                .get()
                .match_name(tap_name.to_string())
                .execute()
                .try_next()
                .await
                .ok()
                .flatten()
                .map(|message| message.header.index)
        }
    }

    fn failed(what: &'static str, device: &str, reason: impl std::fmt::Display) -> NetworkError {
        NetworkError { what, device: device.to_string(), reason: reason.to_string() }
    }

    /// Created persistent through `/dev/net/tun` rather than by netlink, because a tap opened by
    /// a process and not made persistent goes away with the descriptor — and the process that
    /// should own it is the Firecracker this daemon is about to start, not this daemon.
    fn create_persistent_tap(tap_name: &str) -> Result<(), NetworkError> {
        use std::os::fd::AsRawFd;

        const IFF_TAP: libc::c_short = 0x0002;
        const IFF_NO_PI: libc::c_short = 0x1000;
        const TUNSETIFF: libc::c_ulong = 0x400454ca;
        const TUNSETPERSIST: libc::c_ulong = 0x400454cb;

        #[repr(C)]
        struct InterfaceRequest {
            name: [libc::c_char; libc::IFNAMSIZ],
            flags: libc::c_short,
            padding: [u8; 22],
        }

        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|error| failed("a tap device", tap_name, error))?;
        let mut request = InterfaceRequest {
            name: [0; libc::IFNAMSIZ],
            flags: IFF_TAP | IFF_NO_PI,
            padding: [0; 22],
        };
        for (index, byte) in tap_name.as_bytes().iter().take(libc::IFNAMSIZ - 1).enumerate() {
            request.name[index] = *byte as libc::c_char;
        }
        // Safety: the request is the kernel's own `ifreq`, and the descriptor is the tun device.
        let created = unsafe { libc::ioctl(device.as_raw_fd(), TUNSETIFF, &mut request) };
        if created < 0 {
            return Err(failed("a tap device", tap_name, std::io::Error::last_os_error()));
        }
        let persisted = unsafe { libc::ioctl(device.as_raw_fd(), TUNSETPERSIST, 1) };
        if persisted < 0 {
            return Err(failed("a persistent tap device", tap_name, std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Without this the kernel drops replies to the forwarded 127.0.0.1 port as martian, before
    /// the nftables SNAT can rewrite them — and the local proxy reaches every app that way.
    fn allow_route_localnet(tap_name: &str) -> Result<(), NetworkError> {
        let path = format!("/proc/sys/net/ipv4/conf/{tap_name}/route_localnet");
        std::fs::write(&path, b"1").map_err(|error| failed("route_localnet", tap_name, error))
    }

    #[async_trait]
    impl HostNetwork for KernelNetwork {
        async fn ensure_tap(&self, tap: &TapInterface) -> Result<(), NetworkError> {
            if self.index_of(&tap.tap_name).await.is_none() {
                create_persistent_tap(&tap.tap_name)?;
            }
            let index = self
                .index_of(&tap.tap_name)
                .await
                .ok_or_else(|| failed("a tap device", &tap.tap_name, "it did not appear"))?;
            // `replace` rather than `add`, so a converged host re-runs this to no effect.
            self.handle
                .address()
                .add(index, IpAddr::V4(tap.host_ipv4.addr()), tap.subnet_prefix_length)
                .replace()
                .execute()
                .await
                .map_err(|error| failed("an address", &tap.tap_name, error))?;
            self.handle
                .link()
                .set(LinkUnspec::new_with_index(index).up().build())
                .execute()
                .await
                .map_err(|error| failed("bringing the link up", &tap.tap_name, error))?;
            allow_route_localnet(&tap.tap_name)
        }

        async fn refresh_neighbour(&self, neighbour: &Neighbour) -> Result<(), NetworkError> {
            let index = self
                .index_of(&neighbour.tap_name)
                .await
                .ok_or_else(|| failed("a neighbour entry", &neighbour.tap_name, "the tap is not there"))?;
            let mac: Vec<u8> = neighbour
                .guest_mac
                .split(':')
                .filter_map(|octet| u8::from_str_radix(octet, 16).ok())
                .collect();
            self.handle
                .neighbours()
                .add(index, IpAddr::V4(neighbour.guest_ipv4.addr()))
                .link_layer_address(&mac)
                .state(NeighbourState::Reachable)
                .replace()
                .execute()
                .await
                .map_err(|error| failed("a neighbour entry", &neighbour.tap_name, error))
        }

        async fn tap_names(&self) -> Vec<String> {
            let mut names = Vec::new();
            let mut links = self.handle.link().get().execute();
            while let Ok(Some(message)) = links.try_next().await {
                for attribute in message.attributes {
                    if let LinkAttribute::IfName(name) = attribute {
                        if nft_render::is_tap_name(&name) {
                            names.push(name);
                        }
                    }
                }
            }
            names
        }
    }

    /// Only ever built where a tap can be made, so a daemon on any other kernel says so at
    /// startup rather than at the first boot.
    pub fn _assert_send_sync() {
        fn is_send_sync<T: Send + Sync>() {}
        is_send_sync::<KernelNetwork>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn what_a_boot_asks_of_the_network_is_recorded_in_order() {
        let network = RecordingNetwork::new();
        let slot = nft_render::describe_slot(0, protocol::AppId::parse("app-1").unwrap());
        network
            .ensure_tap(&TapInterface {
                tap_name: slot.tap_name.clone(),
                host_ipv4: slot.host_ipv4.clone(),
                subnet_prefix_length: slot.subnet_prefix_length,
            })
            .await
            .unwrap();
        network
            .refresh_neighbour(&Neighbour {
                guest_ipv4: slot.guest_ipv4.clone(),
                guest_mac: slot.guest_mac.clone(),
                tap_name: slot.tap_name.clone(),
            })
            .await
            .unwrap();
        assert_eq!(network.tap_names().await, vec!["nbr0".to_string()]);
        assert_eq!(network.neighbours()[0].guest_mac, "02:00:0a:c9:00:02");
    }
}
