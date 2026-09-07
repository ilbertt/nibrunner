use std::collections::BTreeMap;
use std::path::Path;

use protocol::{HostCapacity, HostId, HostReportedState, HostState, HostVersions};

use crate::clock::now_timestamp;
use crate::host::Host;
use crate::services::report::build_report::{build_reported_state, ReportInputs};
use crate::services::report::capacity::{
    allocatable_capacity, committed_resources, read_filesystem_space, read_vcpu_count,
};

pub async fn host_id_of(host: &Host) -> HostId {
    host.known_host_id()
        .await
        .unwrap_or_else(|| HostId::parse("host-local").expect("a constant identifier"))
}

pub async fn build(host: &Host, versions: HostVersions) -> HostReportedState {
    let snapshot = host.state.snapshot().await;
    let records: Vec<_> = snapshot.records.values().cloned().collect();
    let space = read_filesystem_space(&host.config.state_dir).unwrap_or_default();
    let capacity = HostCapacity {
        vcpu_count: read_vcpu_count(),
        memory_mib: host.guest_memory_mib,
        cache_bytes: space.total_bytes,
    };
    let allocatable = allocatable_capacity(&capacity, &committed_resources(&records), space.available_bytes);

    let mut reached_at = BTreeMap::new();
    if let Some(ipv4) = &host.config.port_relay_public_ipv4 {
        for record in records.iter().filter(|record| record.wants_extra_public_port()) {
            if let Some(slot) = host.slot_of(&record.app_id).await {
                reached_at.insert(
                    record.app_id.clone(),
                    guest_contract::instance_env::PublicAddress {
                        ipv4: ipv4.clone(),
                        port: slot.extra_public_port,
                    },
                );
            }
        }
    }

    build_reported_state(ReportInputs {
        host_id: host_id_of(host).await,
        reported_at: now_timestamp(),
        state: if snapshot.converged {
            HostState::Ready
        } else {
            HostState::Registering
        },
        capacity,
        allocatable,
        versions,
        records: &records,
        reached_at: &reached_at,
        volumes: snapshot.volume_reports.clone(),
        volume_usage: &snapshot.volume_usage,
        compute_usage: &snapshot.compute_usage,
        checkpoints: snapshot.checkpoint_reports.clone(),
        exports: snapshot.export_reports.clone(),
    })
}

pub fn write(path: &Path, report: &HostReportedState) {
    if let Err(error) = crate::json_store::write_json(path, report) {
        tracing::warn!(error = %error.message(), "this host could not write down what it observed");
    }
}

pub fn reported_state_file(host: &Host) -> std::path::PathBuf {
    host.config.in_state_dir("reported.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use protocol::{InstanceState, Ipv4Address};

    const RELAY_IPV4: &str = "203.0.113.10";

    fn versions() -> HostVersions {
        crate::services::report::versions::compiled_versions("v1.16.1", "6.1.180-test")
    }

    async fn host_relaying() -> TestHost {
        let mut host = test_host().await;
        std::sync::Arc::get_mut(&mut host.host)
            .expect("the test holds the only handle on this host")
            .config
            .port_relay_public_ipv4 = Some(Ipv4Address::parse(RELAY_IPV4).unwrap());
        host
    }

    #[tokio::test]
    async fn a_report_says_what_the_host_holds_and_what_is_left_of_it() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        host.state.modify(|snapshot| snapshot.converged = true).await;

        let report = build(&host, versions()).await;
        assert_eq!(report.state, HostState::Ready);
        assert_eq!(report.instances.len(), 1);
        assert_eq!(report.instances[0].app_id, app_id());
        assert_eq!(report.instances[0].state, InstanceState::Running);
        assert_eq!(report.capacity.memory_mib, host.guest_memory_mib);
        assert_eq!(
            report.allocatable.memory_mib,
            host.guest_memory_mib - u64::from(protocol::DEFAULT_INSTANCE_RESOURCES.memory_mib)
        );
    }

    #[tokio::test]
    async fn a_host_with_no_id_of_its_own_still_has_one_to_report_under() {
        let host = test_host().await;
        assert_eq!(host_id_of(&host).await.as_str(), "host-local");
        host.remember_host_id("host-7").await;
        assert_eq!(host_id_of(&host).await.as_str(), "host-7");

        host.remember_host_id("host-9").await;
        assert_eq!(host_id_of(&host).await.as_str(), "host-7");
    }

    #[tokio::test]
    async fn what_is_written_is_the_document_it_will_be_read_as() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        let report = build(&host, versions()).await;
        let path = reported_state_file(&host);
        write(&path, &report);
        let read_back: HostReportedState = crate::json_store::read_json(&path)
            .unwrap()
            .expect("the report is there");
        assert_eq!(read_back, report);
        assert_eq!(read_back.state, HostState::Registering);
    }

    #[tokio::test]
    async fn a_report_that_cannot_be_written_is_logged_rather_than_thrown() {
        let host = test_host().await;
        let path = reported_state_file(&host);
        std::fs::create_dir_all(&path).unwrap();
        write(&path, &build(&host, versions()).await);
        assert!(path.is_dir());
    }

    #[tokio::test]
    async fn an_app_that_asked_for_its_own_port_is_reported_with_the_address_it_answers_on() {
        let host = host_relaying().await;
        let slot = host.slot_for(&app_id()).await.unwrap();
        host.state
            .put_record(instance_record(|record| {
                record.has_extra_public_port = Some(true)
            }))
            .await;

        let report = build(&host, versions()).await;
        assert_eq!(
            report.instances[0].public_ipv4,
            Some(Ipv4Address::parse(RELAY_IPV4).unwrap())
        );
        assert_eq!(
            report.instances[0].extra_public_port,
            Some(slot.extra_public_port)
        );
    }

    #[tokio::test]
    async fn an_app_that_asked_for_nothing_is_not_handed_a_public_address_it_never_wanted() {
        let host = host_relaying().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(instance_record(|_| {})).await;

        let report = build(&host, versions()).await;
        assert_eq!(report.instances[0].public_ipv4, None);
        assert_eq!(report.instances[0].extra_public_port, None);
    }

    #[tokio::test]
    async fn an_app_with_no_slot_of_its_own_is_reported_without_an_address_rather_than_a_guessed_one() {
        let host = host_relaying().await;
        host.state
            .put_record(instance_record(|record| {
                record.has_extra_public_port = Some(true)
            }))
            .await;

        let report = build(&host, versions()).await;
        assert_eq!(report.instances[0].public_ipv4, None);
        assert_eq!(report.instances[0].extra_public_port, None);
    }

    #[tokio::test]
    async fn a_host_that_relays_no_ports_reports_no_public_address_for_an_app_that_asked() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state
            .put_record(instance_record(|record| {
                record.has_extra_public_port = Some(true)
            }))
            .await;

        let report = build(&host, versions()).await;
        assert_eq!(report.instances[0].public_ipv4, None);
    }
}
