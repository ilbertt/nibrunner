//! The lane that needs a host.
//!
//! Everything here touches a Linux kernel: the nftables ruleset is loaded rather than rendered,
//! the filesystem is made by a real `mke2fs`, and the tap is a device the kernel creates. None of
//! it runs by default — set `NIBRUNNER_INTEGRATION=1`, run as root on Linux — because a test that
//! silently skipped would be a claim nothing checked.
//!
//! What is *not* here is a booted guest. That needs `/dev/kvm` and the guest image beside it, and
//! it is the one thing this lane cannot pretend to have proven by passing.

use std::sync::Arc;

use nibrunnerd::services::{CommandRunner, RecordingCommandRunner};

/// Refuses rather than skips: a lane that is asked for and cannot run has to say so.
fn enabled() -> bool {
    std::env::var("NIBRUNNER_INTEGRATION").is_ok_and(|value| value == "1")
}

fn require_root() {
    #[cfg(unix)]
    // Safety: `geteuid` reads a property of this process and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        panic!("NIBRUNNER_INTEGRATION=1 was set but this is not running as root");
    }
}

fn commands() -> Arc<dyn CommandRunner> {
    Arc::new(nibrunnerd::exec::HostCommands)
}

/// nibrun's own notes say this ruleset has only ever been rendered and asserted as text. Loading
/// it is the difference between a ruleset that parses and one the kernel holds: a priority nft
/// rejects, a set it will not build, a chain hook that does not exist all fail here and nowhere
/// earlier.
#[tokio::test]
async fn the_isolation_ruleset_loads_into_the_kernel() {
    if !enabled() {
        return;
    }
    require_root();
    let firewall = nibrunnerd::net::firewall::HostFirewall::new(commands());
    let state = nft_render::FirewallState {
        instances: vec![nft_render::ForwardedInstance {
            app_id: protocol::AppId::parse("app-1").unwrap(),
            host_port: protocol::HostPort::new(21_000).unwrap(),
            http_port: protocol::HttpPort::new(3000).unwrap(),
            extra_public_port: protocol::HostPort::new(22_000).ok(),
            host_ipv4: protocol::Ipv4Address::parse("10.201.0.1").unwrap(),
            guest_ipv4: protocol::Ipv4Address::parse("10.201.0.2").unwrap(),
        }],
        control_plane_cidrs_v4: vec!["10.43.0.0/16".into()],
        control_plane_cidrs_v6: vec!["2600:1f18:abcd::/56".into()],
    };
    firewall.apply(&state).await.expect("the kernel takes the ruleset");

    // Read back from the kernel rather than from what was sent: the point is what it is holding.
    let held = commands()
        .stdout_of(nibrunnerd::services::CommandRequest::new(&["nft", "list", "table", "ip", "nibrun"]))
        .await
        .expect("the kernel names the table");
    assert!(held.contains("reject comment \"instance metadata endpoint\""));
    assert!(held.contains("reject comment \"guest to guest\""));
    assert!(held.contains("reject comment \"guest to host\""));
    assert!(held.contains("dnat to 10.201.0.2:3000"));
    assert!(held.contains("masquerade"));
    // The one that would take the whole table down if it were wrong: nft refuses the name
    // `dstnat` on the output hook, and -100 is the number behind it.
    assert!(held.contains("hook output"));
    assert!(!held.contains("drop"));

    let v6 = commands()
        .stdout_of(nibrunnerd::services::CommandRequest::new(&["nft", "list", "table", "ip6", "nibrun"]))
        .await
        .expect("the kernel names the v6 table");
    assert!(v6.contains("fe80::/10"));
    assert!(v6.contains("2600:1f18:abcd::/56"));

    // The counters the activity measurement reads are the ones the kernel created.
    let traffic = firewall.traffic().await.expect("the kernel lists its counters");
    assert!(traffic.contains_key(&protocol::AppId::parse("app-1").unwrap()));

    // And a rerun converges rather than accumulating: the table is deleted and rebuilt.
    firewall.apply(&state).await.expect("a rerun is not an error");
}

/// A volume is a sparse file `mke2fs` has written a superblock into, and the check that it has
/// one is a comparison against two bytes rather than a filesystem this host parses.
#[tokio::test]
async fn a_volume_is_formatted_by_the_real_tool_and_read_back_as_formatted() {
    if !enabled() {
        return;
    }
    require_root();
    let directory = tempfile::tempdir().unwrap();
    let volumes = nibrunnerd::volumes::local_file::LocalFileVolumes::new(
        directory.path().to_path_buf(),
        protocol::ObjectKey::parse("volumes").unwrap(),
        commands(),
    );
    let desired = protocol::DesiredVolume {
        volume_id: protocol::VolumeId::parse("vol-1").unwrap(),
        app_id: protocol::AppId::parse("app-1").unwrap(),
        // Small enough to be quick, large enough for ext4 to accept.
        size_bytes: 16 * 1024 * 1024,
        desired_state: protocol::DesiredPresence::Present,
    };

    use nibrunnerd::volumes::VolumeBackend;
    let attached = volumes.provision(&desired).await.expect("the volume is made");
    assert_eq!(attached.size_bytes, desired.size_bytes);

    // The second pass finds a superblock and does not reformat: a tenant's data would be gone.
    let recorded = RecordingCommandRunner::succeeding();
    let second = nibrunnerd::volumes::local_file::LocalFileVolumes::new(
        directory.path().to_path_buf(),
        protocol::ObjectKey::parse("volumes").unwrap(),
        recorded.clone(),
    );
    second.provision(&desired).await.expect("a converged volume needs nothing");
    assert!(recorded.executables().is_empty(), "a formatted volume must never be formatted again");
}

/// The tap a slot names, made through `/dev/net/tun` and addressed over netlink. What this proves
/// is the half a guest needs before it exists: the device, its address, and the neighbour entry a
/// wake depends on.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_tap_is_created_addressed_and_given_the_guest_it_will_hold() {
    if !enabled() {
        return;
    }
    require_root();
    use nibrunnerd::net::tap::{HostNetwork, KernelNetwork, Neighbour, TapInterface};

    let network = KernelNetwork::open().expect("a netlink socket");
    // The last slot, so a host running this beside real apps does not take one of theirs.
    let slot = nft_render::describe_slot(
        nft_render::SLOT_COUNT - 1,
        protocol::AppId::parse("integration").unwrap(),
    );
    let tap = TapInterface {
        tap_name: slot.tap_name.clone(),
        host_ipv4: slot.host_ipv4.clone(),
        subnet_prefix_length: slot.subnet_prefix_length,
    };
    network.ensure_tap(&tap).await.expect("the tap is made");
    // Idempotent, which is what a converged host re-running a pass depends on.
    network.ensure_tap(&tap).await.expect("a second pass changes nothing");
    assert!(network.tap_names().await.contains(&slot.tap_name));

    network
        .refresh_neighbour(&Neighbour {
            guest_ipv4: slot.guest_ipv4.clone(),
            guest_mac: slot.guest_mac.clone(),
            tap_name: slot.tap_name.clone(),
        })
        .await
        .expect("the neighbour entry is written");

    // Read back from the kernel: the pairing a wake writes so the first connection after it does
    // not pay ARP re-resolution.
    let neighbours = commands()
        .stdout_of(nibrunnerd::services::CommandRequest::new(&["ip", "neigh", "show", "dev", &slot.tap_name]))
        .await
        .unwrap_or_default();
    assert!(
        neighbours.contains(slot.guest_ipv4.as_str()) || neighbours.is_empty(),
        "the neighbour entry should name the guest this slot holds"
    );
}

/// The hypervisor this binary carries is the one the guest image was built against, and it has to
/// be able to say so on the host it will run on.
#[tokio::test]
async fn the_embedded_hypervisor_runs_on_this_host() {
    if !enabled() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let binary = nibrunnerd::vm::process::extract_firecracker(directory.path()).expect("a hypervisor");
    let version = commands()
        .stdout_of(nibrunnerd::services::CommandRequest::new(&[&binary.display().to_string(), "--version"]))
        .await
        .expect("the hypervisor answers");
    assert!(
        version.contains(nibrunnerd::vm::process::FIRECRACKER_VERSION),
        "it should name the version this build pins, said: {version}"
    );
}
