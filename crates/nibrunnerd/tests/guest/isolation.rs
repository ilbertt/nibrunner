//! Isolation, asked of the kernel rather than of the ruleset's text. Every target here is one
//! something is actually listening on, so a guest that got through says "reached" rather than a
//! refusal that would have happened anyway.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};

use nibrunnerd::adapters::vm::process::VmProcesses;
use nibrunnerd::test_support::egress::EgressEndpoint;
use nibrunnerd::test_support::machine::{RunningHost, Tenant};
use protocol::InstanceState;
use std::os::unix::fs::MetadataExt;

/// The tenant gives up after two seconds, so anything under this came back because a rule said
/// no rather than because nothing was there.
const REJECTED_WITHIN_MS: u128 = 1_500;

async fn reaching(host: &RunningHost, app: &Tenant, address: &str) -> String {
    host.get(app, &format!("/reach?addr={address}"))
        .await
        .expect("the proxy answers")
        .body
}

fn said_milliseconds(said: &str) -> u128 {
    said.split_once(" in ")
        .and_then(|(_, rest)| rest.split_once("ms"))
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or(u128::MAX)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_cannot_reach_another_app_on_this_host() {
    let Some(host) = crate::host().await else {
        return;
    };
    let one = host.tenant(1);
    let another = host.tenant(2);
    host.deploy(&[one.clone(), another.clone()]).await;
    host.until_state(&one.app_id, InstanceState::Running).await;
    host.until_state(&another.app_id, InstanceState::Running).await;

    // The other guest's tenant is listening on this exact address and port, so nothing but the
    // ruleset can be what stops this.
    let neighbour = host
        .host
        .slot_of(&another.app_id)
        .await
        .expect("a slot")
        .guest_ipv4;
    let said = reaching(
        &host,
        &one,
        &format!("{}:{}", neighbour.as_str(), protocol::DEFAULT_HTTP_PORT),
    )
    .await;
    assert!(said.starts_with("blocked"), "one app reached another: {said}");

    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_cannot_reach_the_host_it_runs_on() {
    let Some(host) = crate::host().await else {
        return;
    };
    let app = host.tenant(1);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;

    let slot = host.host.slot_of(&app.app_id).await.expect("a slot");
    let side: Ipv4Addr = slot
        .host_ipv4
        .as_str()
        .parse()
        .expect("the host's side of the tap");
    let listening = TcpListener::bind(SocketAddr::from((side, 0))).expect("a listener on the tap");
    let address = listening.local_addr().expect("its address");
    std::thread::spawn(move || for _ in listening.incoming() {});

    let said = reaching(&host, &app, &address.to_string()).await;
    assert!(
        said.starts_with("blocked"),
        "an app reached something listening on its host: {said}"
    );

    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_cannot_reach_the_instance_metadata_endpoint() {
    let Some(host) = crate::host().await else {
        return;
    };
    let app = host.tenant(1);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;

    let said = reaching(&host, &app, "169.254.169.254:80").await;
    assert!(
        said.starts_with("blocked"),
        "the metadata endpoint answered: {said}"
    );
    assert!(
        said_milliseconds(&said) < REJECTED_WITHIN_MS,
        "nothing rejected it — it was left to time out, which is not the same rule: {said}"
    );

    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_address_the_configuration_denies_cannot_be_reached() {
    let Some(open) = crate::host().await else {
        return;
    };
    let endpoint = EgressEndpoint::start();
    let target = endpoint.address().to_string();
    let app = open.tenant(1);
    open.deploy(std::slice::from_ref(&app)).await;
    open.until_state(&app.app_id, InstanceState::Running).await;
    open.until_routed(&app).await;
    let allowed = reaching(&open, &app, &target).await;
    assert!(
        allowed.starts_with("reached"),
        "the positive control failed: {allowed}"
    );
    open.stop().await;

    // A second host, the same in every way but the range it refuses.
    let Some(closed) = crate::host_with(|config| {
        config.denied_egress_addresses_v4 = vec![endpoint.denied_cidr()];
    })
    .await
    else {
        return;
    };
    let app = closed.tenant(1);
    closed.deploy(std::slice::from_ref(&app)).await;
    closed.until_state(&app.app_id, InstanceState::Running).await;
    closed.until_routed(&app).await;

    let said = reaching(&closed, &app, &target).await;
    assert!(
        said.starts_with("blocked"),
        "a denied address was reached: {said}"
    );
    assert!(
        said_milliseconds(&said) < REJECTED_WITHIN_MS,
        "the deny timed out: {said}"
    );

    closed.stop().await;
}

const JAILER_IDLE_TIMEOUT_MS: u64 = 60_000;

#[tokio::test(flavor = "multi_thread")]
async fn jailed_vmm_processes_have_distinct_non_root_identities_and_resume_their_memory_and_volume() {
    let Some(host) = crate::host().await else {
        return;
    };
    let one = host.tenant(1).on_request(JAILER_IDLE_TIMEOUT_MS);
    let another = host.tenant(2);
    host.deploy(&[one.clone(), another.clone()]).await;
    host.until_state(&one.app_id, InstanceState::Running).await;
    host.until_state(&another.app_id, InstanceState::Running).await;
    let processes = VmProcesses::new(host.host.config.runtime_dir.clone());
    let first = processes.read_record(&one.app_id).expect("the first VMM record");
    let second = processes
        .read_record(&another.app_id)
        .expect("the second VMM record");
    for record in [&first, &second] {
        let root = std::fs::metadata(record.jail_root.as_ref().expect("the recorded jail root"))
            .expect("the jail root's owner");
        assert_eq!(record.jail_uid, Some(root.uid()));
        let status = std::fs::read_to_string(format!("/proc/{}/status", record.pid))
            .expect("the VMM's process status");
        for (label, identity) in [("Uid:", root.uid()), ("Gid:", root.gid())] {
            assert_ne!(identity, 0);
            let line = status
                .lines()
                .find(|line| line.starts_with(label))
                .expect("the process identity line");
            assert!(
                line.split_whitespace()
                    .skip(1)
                    .all(|value| value.parse::<u32>() == Ok(identity)),
                "{line}"
            );
        }
    }
    let root = first.jail_root.as_ref().expect("the recorded jail root");
    let process_root = std::path::PathBuf::from(format!("/proc/{}/root", first.pid));
    let actual = std::fs::metadata(&process_root).expect("the VMM root");
    let recorded = std::fs::metadata(root).expect("the recorded root inode");
    assert_eq!((actual.dev(), actual.ino()), (recorded.dev(), recorded.ino()));
    assert!(
        std::fs::metadata(process_root.join("assets/kernel"))
            .unwrap()
            .len()
            > 0
    );
    assert_ne!(first.jail_root, second.jail_root);
    let other_root = second.jail_root.as_ref().expect("the second jail");
    assert_ne!(
        std::fs::metadata(root).unwrap().uid(),
        std::fs::metadata(other_root).unwrap().uid()
    );
    assert_ne!(
        std::fs::metadata(root).unwrap().gid(),
        std::fs::metadata(other_root).unwrap().gid()
    );
    assert!(!process_root.join("etc/nibrunner/config.toml").exists());
    assert_eq!(
        host.get(&one, "/remember")
            .await
            .expect("memory before sleep")
            .body,
        "1"
    );
    assert_eq!(
        host.get(&one, "/write?path=kept&body=jailed")
            .await
            .expect("a volume write")
            .status,
        200
    );
    host.let_sleep(&one).await;
    assert_eq!(
        host.get(&one, "/remember").await.expect("memory after wake").body,
        "2"
    );
    assert_eq!(
        host.get(&one, "/read?path=kept")
            .await
            .expect("the volume after wake")
            .body,
        "jailed"
    );
    host.host
        .vms
        .readopt(&one.app_id)
        .await
        .expect("reattach jailed host channels");
    assert_eq!(host.get(&one, "/").await.expect("the readopted app").status, 200);
    host.stop().await;
}
