#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::sync::Arc;

use nibrunnerd::ports::{CommandRunner, CommandRunnerExt};
use nibrunnerd::test_support::mocks;

fn enabled() -> bool {
    std::env::var("NIBRUNNER_INTEGRATION").is_ok_and(|value| value == "1")
}

fn require_root() {
    #[cfg(unix)]
    #[allow(unsafe_code, reason = "asking who this process is has no safe spelling")]
    if unsafe { libc::geteuid() } != 0 {
        panic!("NIBRUNNER_INTEGRATION=1 was set but this is not running as root");
    }
}

fn commands() -> Arc<dyn CommandRunner> {
    Arc::new(nibrunnerd::adapters::exec::HostCommands)
}

#[tokio::test]
async fn the_isolation_ruleset_loads_into_the_kernel() {
    if !enabled() {
        return;
    }
    require_root();
    let firewall = nibrunnerd::adapters::net::firewall::HostFirewall::new(commands());
    let state = nft_render::FirewallState {
        instances: vec![nft_render::ForwardedInstance {
            app_id: protocol::AppId::parse("app-1").unwrap(),
            host_port: protocol::HostPort::new(21_000).unwrap(),
            http_port: protocol::HttpPort::new(3000).unwrap(),
            host_ipv4: protocol::Ipv4Address::parse("10.201.0.1").unwrap(),
            guest_ipv4: protocol::Ipv4Address::parse("10.201.0.2").unwrap(),
        }],
        control_plane_cidrs_v4: vec!["10.43.0.0/16".into()],
        control_plane_cidrs_v6: vec!["2600:1f18:abcd::/56".into()],
    };
    firewall
        .apply(&state)
        .await
        .expect("the kernel takes the ruleset");

    let held = commands()
        .stdout_of(nibrunnerd::ports::CommandRequest::new(&[
            "nft", "list", "table", "ip", "nibrun",
        ]))
        .await
        .expect("the kernel names the table");
    assert!(held.contains("reject comment \"instance metadata endpoint\""));
    assert!(held.contains("reject comment \"guest to guest\""));
    assert!(held.contains("reject comment \"guest to host\""));
    assert!(held.contains("dnat to 10.201.0.2:3000"));
    assert!(held.contains("masquerade"));
    assert!(held.contains("hook output"));
    assert!(!held.contains("drop"));

    let v6 = commands()
        .stdout_of(nibrunnerd::ports::CommandRequest::new(&[
            "nft", "list", "table", "ip6", "nibrun",
        ]))
        .await
        .expect("the kernel names the v6 table");
    assert!(v6.contains("fe80::/10"));
    assert!(v6.contains("2600:1f18:abcd::/56"));

    let traffic = firewall.traffic().await.expect("the kernel lists its counters");
    assert!(traffic.contains_key(&protocol::AppId::parse("app-1").unwrap()));

    firewall.apply(&state).await.expect("a rerun is not an error");
}

#[tokio::test]
async fn a_volume_is_formatted_by_the_real_tool_and_read_back_as_formatted() {
    if !enabled() {
        return;
    }
    require_root();
    let directory = tempfile::tempdir().unwrap();
    let volumes = nibrunnerd::adapters::volumes::local_file::LocalFileVolumes::new(
        directory.path().to_path_buf(),
        protocol::ObjectKey::parse("volumes").unwrap(),
        commands(),
    );
    let desired = protocol::DesiredVolume {
        volume_id: protocol::VolumeId::parse("vol-1").unwrap(),
        app_id: protocol::AppId::parse("app-1").unwrap(),
        size_bytes: 16 * 1024 * 1024,
        desired_state: protocol::DesiredPresence::Present,
    };

    use nibrunnerd::adapters::volumes::VolumeBackend;
    let attached = volumes.provision(&desired).await.expect("the volume is made");
    assert_eq!(attached.size_bytes, desired.size_bytes);

    let (recorded, log) = mocks::commands_succeeding();
    let second = nibrunnerd::adapters::volumes::local_file::LocalFileVolumes::new(
        directory.path().to_path_buf(),
        protocol::ObjectKey::parse("volumes").unwrap(),
        recorded,
    );
    second
        .provision(&desired)
        .await
        .expect("a converged volume needs nothing");
    assert!(
        log.executables().is_empty(),
        "a formatted volume must never be formatted again"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_tap_is_created_addressed_and_given_the_guest_it_will_hold() {
    if !enabled() {
        return;
    }
    require_root();
    use nibrunnerd::adapters::net::tap::{HostNetwork, KernelNetwork, Neighbour, TapInterface};

    let network = KernelNetwork::open().expect("a netlink socket");
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
    network
        .ensure_tap(&tap)
        .await
        .expect("a second pass changes nothing");
    assert!(network.tap_names().await.contains(&slot.tap_name));

    network
        .refresh_neighbour(&Neighbour {
            guest_ipv4: slot.guest_ipv4.clone(),
            guest_mac: slot.guest_mac.clone(),
            tap_name: slot.tap_name.clone(),
        })
        .await
        .expect("the neighbour entry is written");

    let neighbours = commands()
        .stdout_of(nibrunnerd::ports::CommandRequest::new(&[
            "ip",
            "neigh",
            "show",
            "dev",
            &slot.tap_name,
        ]))
        .await
        .unwrap_or_default();
    assert!(
        neighbours.contains(slot.guest_ipv4.as_str()) || neighbours.is_empty(),
        "the neighbour entry should name the guest this slot holds"
    );
}

#[tokio::test]
async fn the_embedded_hypervisor_runs_on_this_host() {
    if !enabled() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let binary =
        nibrunnerd::adapters::vm::process::extract_firecracker(directory.path()).expect("a hypervisor");
    let version = commands()
        .stdout_of(nibrunnerd::ports::CommandRequest::new(&[
            &binary.display().to_string(),
            "--version",
        ]))
        .await
        .expect("the hypervisor answers");
    assert!(
        version.contains(nibrunnerd::adapters::vm::process::FIRECRACKER_VERSION),
        "it should name the version this build pins, said: {version}"
    );
}
