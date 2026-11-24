#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::process::Command;

use nft_render::egress::{egress_listing, egress_transaction, parse_egress, EgressBindings, EgressCounts};
use protocol::AppId;

#[test]
fn normal_connections_and_refusals_follow_the_tap_lease_across_firewall_replacement_and_reuse() {
    if std::env::var("NIBRUNNER_INTEGRATION").as_deref() != Ok("1") {
        return;
    }
    let a = AppId::parse("first-app").unwrap();
    let b = AppId::parse("second-app").unwrap();
    let initial: EgressBindings = [(a.clone(), Some(0))].into();
    let reused: EgressBindings = [(a.clone(), None), (b.clone(), Some(0))].into();
    let apps: BTreeSet<_> = reused.keys().cloned().collect();
    let output = Command::new("unshare")
        .args([
            "--mount-proc",
            "-n",
            "sh",
            "-ec",
            include_str!("fixtures/egress-traffic.sh"),
        ])
        .env("ISOLATION", nft_render::render_ruleset(&Default::default()))
        .env("LISTING", egress_listing(&apps))
        .env("INITIAL", egress_transaction(None, &initial))
        .env("REUSED", egress_transaction(Some(&initial), &reused))
        .env(
            "DETACHED",
            egress_transaction(Some(&reused), &[(b.clone(), Some(0))].into()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let counts = parse_egress(std::str::from_utf8(&output.stdout).unwrap(), &apps).unwrap();
    assert_eq!(
        counts[&a],
        EgressCounts {
            attempts: 5,
            passed: 3,
            destinations: 3,
            unrecorded: 0
        }
    );
    assert_eq!(
        counts[&b],
        EgressCounts {
            attempts: 1,
            passed: 1,
            destinations: 1,
            unrecorded: 0
        }
    );
}
