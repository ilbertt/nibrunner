use protocol::{AppId, HostPort, Ipv4Address};

pub const FIRST_SLOT: u32 = 0;

const NBD_DEVICE_COUNT: u32 = 64;

fn nbd_device_path(minor: u32) -> String {
    format!("/dev/nbd{minor}")
}

pub fn export_reader_device_path() -> String {
    nbd_device_path(NBD_DEVICE_COUNT - 1)
}

/// The zerofs backend addresses one nbd minor per slot and the export reader holds the last
/// of them, so a host on that backend cannot reach the slots the port layout otherwise allows.
pub const NBD_SLOT_LIMIT: u32 = NBD_DEVICE_COUNT - 1;

pub const HOST_PORT_BASE: u16 = 21_000;

/// How many host ports a slot reserves, whether or not an app asks for them.
///
/// A host port is derived from the slot rather than stored, so this stride is what an app's ports
/// are *at*: changing it moves every app on the host at once, which is the one thing the slot
/// table exists to prevent. So it is set wide enough to outlast the number of ports a host allows
/// an app to declare — `proxy.tcp.ports_per_app`, which this bounds — and raising that limit is an
/// edit rather than a migration. A reserved port is not an open one: nothing binds or forwards a
/// port no document named.
pub const PORTS_PER_SLOT: u32 = 8;

const _: () = assert!(
    PORTS_PER_SLOT > 1,
    "a slot with no room beside the HTTP port could offer no way in"
);

/// Only the zerofs backend addresses an nbd minor, so a local-file host is not bounded by how
/// many of those exist. What bounds every host is the range of loopback ports reserved from
/// `HOST_PORT_BASE`, which is this many slots' worth and no more.
pub const SLOT_COUNT: u32 = 1_000;

/// The host ports no listener of an operator's may take, first and last inclusive.
pub fn reserved_port_range() -> (u16, u16) {
    let last = u32::from(HOST_PORT_BASE) + SLOT_COUNT * PORTS_PER_SLOT - 1;
    (
        HOST_PORT_BASE,
        u16::try_from(last).expect("the slot range is laid out to fit a port"),
    )
}

pub const GUEST_NETWORK_CIDR: &str = "10.201.0.0/16";
const GUEST_SUBNET_PREFIX_LENGTH: u8 = 30;
const ADDRESSES_PER_SLOT: u32 = 4;
const GUEST_NETWORK_FIRST_OCTET: u32 = 10;
const GUEST_NETWORK_SECOND_OCTET: u32 = 201;
const OCTET_SIZE: u32 = 256;
const HOST_ADDRESS_OFFSET: u32 = 1;
const GUEST_ADDRESS_OFFSET: u32 = 2;

pub const TAP_NAME_PREFIX: &str = "nbr";

const MAC_PREFIX: &str = "02:00";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSlot {
    pub slot: u32,
    pub app_id: AppId,
    pub host_port: HostPort,
    pub host_ipv4: Ipv4Address,
    pub guest_ipv4: Ipv4Address,
    pub guest_mac: String,
    pub tap_name: String,
    pub nbd_device_path: String,
    pub subnet_prefix_length: u8,
}

fn address_at(index: u32) -> Ipv4Address {
    Ipv4Address::parse(format!(
        "{GUEST_NETWORK_FIRST_OCTET}.{GUEST_NETWORK_SECOND_OCTET}.{}.{}",
        index / OCTET_SIZE,
        index % OCTET_SIZE
    ))
    .expect("a slot address is always an address")
}

fn mac_for(address: &Ipv4Address) -> String {
    let octets = address
        .as_str()
        .split('.')
        .map(|octet| format!("{:02x}", octet.parse::<u8>().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(":");
    format!("{MAC_PREFIX}:{octets}")
}

/// The `index`th host port of a slot, or nothing when the slot reserves no such port.
pub fn host_port_at(slot: u32, index: u32) -> Option<HostPort> {
    if index >= PORTS_PER_SLOT {
        return None;
    }
    HostPort::try_from(u32::from(HOST_PORT_BASE) + slot * PORTS_PER_SLOT + index).ok()
}

pub fn describe_slot(slot: u32, app_id: AppId) -> AppSlot {
    let base = slot * ADDRESSES_PER_SLOT;
    let guest_ipv4 = address_at(base + GUEST_ADDRESS_OFFSET);
    AppSlot {
        slot,
        app_id,
        host_port: host_port_at(slot, 0).expect("the first port of a slot is always in range"),
        host_ipv4: address_at(base + HOST_ADDRESS_OFFSET),
        guest_mac: mac_for(&guest_ipv4),
        guest_ipv4,
        tap_name: format!("{TAP_NAME_PREFIX}{slot}"),
        nbd_device_path: nbd_device_path(slot),
        subnet_prefix_length: GUEST_SUBNET_PREFIX_LENGTH,
    }
}

impl AppSlot {
    /// The `index`th host port this slot reserves. Index 0 is [`AppSlot::host_port`].
    pub fn host_port_at(&self, index: u32) -> Option<HostPort> {
        host_port_at(self.slot, index)
    }
}

pub fn is_tap_name(name: &str) -> bool {
    name.strip_prefix(TAP_NAME_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppId {
        AppId::parse(format!("app-{name}")).unwrap()
    }

    #[test]
    fn every_per_app_resource_comes_from_the_one_number() {
        let slot = describe_slot(0, app("0"));
        assert_eq!(slot.host_port.get(), HOST_PORT_BASE);
        assert_eq!(slot.host_ipv4.as_str(), "10.201.0.1");
        assert_eq!(slot.guest_ipv4.as_str(), "10.201.0.2");
        assert_eq!(slot.guest_mac, "02:00:0a:c9:00:02");
        assert_eq!(slot.tap_name, "nbr0");
        assert_eq!(slot.nbd_device_path, "/dev/nbd0");
        assert_eq!(slot.subnet_prefix_length, 30);
    }

    #[test]
    fn slots_do_not_overlap_and_carry_past_an_octet_boundary() {
        let first = describe_slot(0, app("0"));
        let second = describe_slot(1, app("1"));
        assert_eq!(second.host_ipv4.as_str(), "10.201.0.5");
        assert_eq!(second.guest_ipv4.as_str(), "10.201.0.6");
        assert_eq!(
            second.host_port.get(),
            first.host_port.get() + u16::try_from(PORTS_PER_SLOT).unwrap(),
            "a slot's ports are its own, so the next slot starts past the whole stride"
        );
        assert_eq!(describe_slot(64, app("64")).guest_ipv4.as_str(), "10.201.1.2");
        assert_eq!(export_reader_device_path(), "/dev/nbd63");
        assert_eq!(SLOT_COUNT, 1_000);
    }

    #[test]
    fn a_slot_reaches_every_port_it_reserves_and_never_the_next_slots() {
        let first = describe_slot(0, app("0"));
        let second = describe_slot(1, app("1"));
        assert_eq!(first.host_port_at(0), Some(first.host_port));
        assert_eq!(
            first.host_port_at(PORTS_PER_SLOT - 1).unwrap().get(),
            second.host_port.get() - 1
        );
        assert_eq!(
            first.host_port_at(PORTS_PER_SLOT),
            None,
            "the port after the last one this slot reserves belongs to the next slot"
        );
    }

    #[test]
    fn the_reserved_range_covers_every_port_every_slot_could_hand_out() {
        let (base, end) = reserved_port_range();
        assert_eq!(base, HOST_PORT_BASE);
        let last = describe_slot(SLOT_COUNT - 1, app("last"));
        assert_eq!(last.host_port_at(PORTS_PER_SLOT - 1).unwrap().get(), end);
    }

    #[test]
    fn tap_names_are_the_prefix_and_a_number() {
        assert!(is_tap_name("nbr12"));
        assert!(!is_tap_name("nbr"));
        assert!(!is_tap_name("eth0"));
    }
}
