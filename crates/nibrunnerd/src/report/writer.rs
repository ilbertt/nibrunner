//! What this host says about itself, written where anything can read it.
//!
//! A file rather than an endpoint, for the reason the input is one: what wants to know what a
//! host is doing reads it, and what wants to send it onwards reads it and posts it. Neither has
//! to hold a connection to this daemon, and a daemon that is not running still leaves the last
//! thing it observed behind.

use std::collections::BTreeMap;
use std::path::Path;

use protocol::{HostCapacity, HostId, HostReportedState, HostState, HostVersions};

use crate::clock::now_timestamp;
use crate::host::Host;
use crate::report::build_report::{build_reported_state, ReportInputs};
use crate::report::capacity::{
    allocatable_capacity, committed_resources, read_filesystem_space, read_vcpu_count,
};

/// A host that has never been given an id of its own is still a host: it reports under one it
/// derives from where it keeps its state, so a report is readable before anything has registered
/// it. A control plane that assigns one writes it here and this reads it back.
pub fn host_id_of(host: &Host) -> HostId {
    crate::json_store::read_text(&host.config.host_id_file())
        .ok()
        .flatten()
        .and_then(|value| HostId::parse(value).ok())
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

    // Where each app that asked for a public port answers, resolved here rather than kept on the
    // record: the port belongs to the slot the app holds and the address to whatever relay this
    // host was configured with, so reading both now is what makes giving a port up show up in the
    // next report rather than at the next boot.
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
        host_id: host_id_of(host),
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
        exports: Vec::new(),
    })
}

/// Written the way every other note this daemon keeps is: through a sibling and a rename, so a
/// reader never sees half a document.
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
    use protocol::InstanceState;

    fn versions() -> HostVersions {
        crate::report::versions::compiled_versions("v1.16.1", "6.1.180-test")
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
        // One running app of the default size, out of the four this host has room for.
        assert_eq!(report.capacity.memory_mib, host.guest_memory_mib);
        assert_eq!(
            report.allocatable.memory_mib,
            host.guest_memory_mib - u64::from(protocol::DEFAULT_INSTANCE_RESOURCES.memory_mib)
        );
    }

    /// A host nobody has registered still reports, because a report nobody can read is a host
    /// nobody can see.
    #[tokio::test]
    async fn a_host_with_no_id_of_its_own_still_has_one_to_report_under() {
        let host = test_host().await;
        assert_eq!(host_id_of(&host).as_str(), "host-local");
        crate::json_store::write_text(&host.config.host_id_file(), "host-7\n", 0o600).unwrap();
        assert_eq!(host_id_of(&host).as_str(), "host-7");
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
        // Before anything converges the host says so, rather than claiming to be ready.
        assert_eq!(read_back.state, HostState::Registering);
    }
}
