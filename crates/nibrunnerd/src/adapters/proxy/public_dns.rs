use std::io;
use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

pub(super) struct PublicDns;

impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0)).await?;
            Ok(checked_addresses(addresses)?)
        })
    }
}

fn checked_addresses(addresses: impl Iterator<Item = SocketAddr>) -> io::Result<Addrs> {
    let addresses: Vec<_> = addresses.collect();
    if addresses.is_empty() || addresses.iter().any(|address| !is_public(address.ip())) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "JWKS hostname must resolve only to public addresses",
        ));
    }
    Ok(Box::new(addresses.into_iter()))
}

// The HTTP connector handles numeric hosts without invoking its DNS resolver.
pub(super) fn allows_literal(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .map_or(true, is_public)
    })
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && !matches!(
                    ip.octets(),
                    [0 | 224..=255, ..]
                        | [100, 64..=127, ..]
                        | [192, 0, 0, _]
                        | [192, 88, 99, _]
                        | [198, 18..=19, ..]
                )
        }
        IpAddr::V6(ip) => {
            let [first, second, ..] = ip.segments();
            (0x2000..=0x3fff).contains(&first)
                && !matches!(
                    (first, second),
                    (0x2001, 0..=0x1ff | 0xdb8) | (0x2002, _) | (0x3fff, 0..=0x0fff)
                )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_checked_dns_answers_are_returned_to_the_connector() {
        let addresses: [SocketAddr; 2] = [
            "93.184.216.34:0".parse().unwrap(),
            "[2606:4700::1111]:0".parse().unwrap(),
        ];
        assert_eq!(
            checked_addresses(addresses.into_iter())
                .unwrap()
                .collect::<Vec<_>>(),
            addresses
        );
        let local = "127.0.0.1:0".parse().unwrap();
        assert!(checked_addresses(addresses.into_iter().chain([local])).is_err());
        assert!(checked_addresses(std::iter::empty()).is_err());
    }

    #[test]
    fn literals_and_resolved_ips_share_the_public_address_policy() {
        for host in [
            "0.0.0.0",
            "127.0.0.1",
            "10.1.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "224.0.0.1",
            "[::1]",
            "[::ffff:10.0.0.1]",
            "[fc00::1]",
            "[fe80::1]",
            "[ff02::1]",
            "[64:ff9b::1]",
            "[2001::1]",
            "[2002::1]",
            "[2001:db8::1]",
            "[3fff::1]",
        ] {
            let url = reqwest::Url::parse(&format!("https://{host}/keys")).unwrap();
            assert!(!allows_literal(&url), "{host}");
        }
    }
}
