use protocol::{AppId, HostPort, Ipv4Address};

/// A host port, a tap, a /30 and an NBD minor all derive from one small integer, so there is one
/// number to persist and no way for three of the four to survive a restart while the fourth does not.
pub const FIRST_SLOT: u32 = 0;

/// `nbds_max` on the nbd module, which decides how many `/dev/nbdN` the kernel creates. It is read
/// once when the module loads, so on a running host it is a ceiling rather than a setting.
const NBD_DEVICE_COUNT: u32 = 64;

fn nbd_device_path(minor: u32) -> String {
    format!("/dev/nbd{minor}")
}

/// Held back from the app range, because an export reads a checkpoint served by a second reader
/// and that needs a device the live volume is not already on. Reserved rather than taken from the
/// free ones on the day: an app's slot persists and an export's does not.
pub fn export_reader_device_path() -> String {
    nbd_device_path(NBD_DEVICE_COUNT - 1)
}

/// Everything below the reader's device: ports and taps are cheap, and the minors are the ceiling.
pub const SLOT_COUNT: u32 = NBD_DEVICE_COUNT - 1;

pub const HOST_PORT_BASE: u16 = 21_000;

/// Where the port an app asking for one is reached at starts. A slot's own, so nothing has to be
/// allocated or told apart, and the same number on both sides of every hop.
pub const EXTRA_PUBLIC_PORT_BASE: u16 = 22_000;

/// A /30 per slot: .0 network, .1 host, .2 guest, .3 broadcast.
pub const GUEST_NETWORK_CIDR: &str = "10.201.0.0/16";
const GUEST_SUBNET_PREFIX_LENGTH: u8 = 30;
const ADDRESSES_PER_SLOT: u32 = 4;
const GUEST_NETWORK_FIRST_OCTET: u32 = 10;
const GUEST_NETWORK_SECOND_OCTET: u32 = 201;
const OCTET_SIZE: u32 = 256;
const HOST_ADDRESS_OFFSET: u32 = 1;
const GUEST_ADDRESS_OFFSET: u32 = 2;

pub const TAP_NAME_PREFIX: &str = "nbr";

/// Locally administered and unicast, with the guest address in the last four octets.
const MAC_PREFIX: &str = "02:00";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSlot {
    pub slot: u32,
    pub app_id: AppId,
    pub host_port: HostPort,
    pub extra_public_port: HostPort,
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

pub fn describe_slot(slot: u32, app_id: AppId) -> AppSlot {
    let base = slot * ADDRESSES_PER_SLOT;
    let guest_ipv4 = address_at(base + GUEST_ADDRESS_OFFSET);
    AppSlot {
        slot,
        app_id,
        host_port: HostPort::try_from(u32::from(HOST_PORT_BASE) + slot).expect("within range"),
        extra_public_port: HostPort::try_from(u32::from(EXTRA_PUBLIC_PORT_BASE) + slot)
            .expect("within range"),
        host_ipv4: address_at(base + HOST_ADDRESS_OFFSET),
        guest_mac: mac_for(&guest_ipv4),
        guest_ipv4,
        tap_name: format!("{TAP_NAME_PREFIX}{slot}"),
        nbd_device_path: nbd_device_path(slot),
        subnet_prefix_length: GUEST_SUBNET_PREFIX_LENGTH,
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
        assert_eq!(slot.extra_public_port.get(), EXTRA_PUBLIC_PORT_BASE);
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
        assert_eq!(second.host_port.get(), first.host_port.get() + 1);
        assert_eq!(describe_slot(64, app("64")).guest_ipv4.as_str(), "10.201.1.2");
        assert_eq!(export_reader_device_path(), "/dev/nbd63");
        assert_eq!(SLOT_COUNT, 63);
    }

    #[test]
    fn tap_names_are_the_prefix_and_a_number() {
        assert!(is_tap_name("nbr12"));
        assert!(!is_tap_name("nbr"));
        assert!(!is_tap_name("eth0"));
    }
}
