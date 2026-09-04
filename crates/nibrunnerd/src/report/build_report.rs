use std::collections::BTreeMap;

use protocol::{
    AppId, ComputeUsage, FilesystemUsage, HostCapacity, HostId, HostReportedState, HostState, HostVersions,
    ReportedCheckpoint, ReportedExport, ReportedInstance, ReportedVolume, Timestamp,
};

use crate::report::InstanceRecord;
use guest_contract::instance_env::PublicAddress;

/// Optional fields are omitted rather than sent empty: absent is the one convention for unknown.
pub fn to_reported_instance(
    record: &InstanceRecord,
    reached_at: Option<&PublicAddress>,
    measured: Option<&ComputeUsage>,
) -> ReportedInstance {
    ReportedInstance {
        app_id: record.app_id.clone(),
        deployment_id: record.deployment_id.clone(),
        state: record.state,
        host_port: Some(record.host_port),
        guest_ipv4: Some(record.guest_ipv4.clone()),
        public_ipv4: reached_at.map(|address| address.ipv4.clone()),
        extra_public_port: reached_at.map(|address| address.port),
        artifact_digest: Some(record.artifact_digest.clone()),
        restart_count: record.restart_count,
        started_at: record.started_at.clone(),
        last_healthy_at: record.health.last_healthy_at.clone(),
        last_exit_code: record.last_exit_code,
        compute: measured.cloned(),
        message: record.message.clone(),
    }
}

/// Stitched onto the report rather than onto the observation the reports are built from: a volume
/// is observed by looking at this host's own storage, and how full the filesystem on it is can
/// only be had by asking the guest — which the reconcile must never be made to wait for.
fn with_usage(volume: ReportedVolume, measured: Option<&FilesystemUsage>) -> ReportedVolume {
    match measured {
        None => volume,
        Some(usage) => ReportedVolume {
            usage: Some(usage.clone()),
            ..volume
        },
    }
}

pub struct ReportInputs<'a> {
    pub host_id: HostId,
    pub reported_at: Timestamp,
    pub state: HostState,
    pub capacity: HostCapacity,
    pub allocatable: HostCapacity,
    pub versions: HostVersions,
    pub records: &'a [InstanceRecord],
    /// Where each app that asked for a public port answers, which is not on the record it is about.
    pub reached_at: &'a BTreeMap<AppId, PublicAddress>,
    pub volumes: Vec<ReportedVolume>,
    pub volume_usage: &'a BTreeMap<AppId, FilesystemUsage>,
    pub compute_usage: &'a BTreeMap<AppId, ComputeUsage>,
    pub checkpoints: Vec<ReportedCheckpoint>,
    pub exports: Vec<ReportedExport>,
}

pub fn build_reported_state(inputs: ReportInputs<'_>) -> HostReportedState {
    HostReportedState {
        host_id: inputs.host_id,
        reported_at: inputs.reported_at,
        state: inputs.state,
        capacity: inputs.capacity,
        allocatable: inputs.allocatable,
        versions: inputs.versions,
        volumes: inputs
            .volumes
            .into_iter()
            .map(|volume| {
                let measured = inputs.volume_usage.get(&volume.app_id);
                with_usage(volume, measured)
            })
            .collect(),
        instances: inputs
            .records
            .iter()
            .map(|record| {
                to_reported_instance(
                    record,
                    inputs.reached_at.get(&record.app_id),
                    inputs.compute_usage.get(&record.app_id),
                )
            })
            .collect(),
        checkpoints: inputs.checkpoints,
        exports: inputs.exports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use protocol::{HostPort, Ipv4Address, VolumeState};

    fn reported_volume() -> ReportedVolume {
        ReportedVolume {
            volume_id: volume_id(),
            app_id: app_id(),
            state: VolumeState::Ready,
            size_bytes: VOLUME_SIZE_BYTES,
            storage_prefix: None,
            device_path: None,
            usage: None,
            message: None,
        }
    }

    fn measured() -> FilesystemUsage {
        FilesystemUsage {
            total_bytes: 8_455_712_768,
            used_bytes: 1_503_238_553,
            measured_at: observed_at(),
        }
    }

    fn report_with(volume_usage: BTreeMap<AppId, FilesystemUsage>) -> HostReportedState {
        let capacity = HostCapacity {
            vcpu_count: 4,
            memory_mib: 8192,
            cache_bytes: 1000,
        };
        build_reported_state(ReportInputs {
            host_id: host_id(),
            reported_at: observed_at(),
            state: HostState::Ready,
            capacity,
            allocatable: capacity,
            versions: HostVersions {
                agent: "sha".into(),
                guest_image: "6.1".into(),
                zerofs: "none".into(),
                firecracker: "v1.16.1".into(),
            },
            records: &[],
            reached_at: &BTreeMap::new(),
            volumes: vec![reported_volume()],
            volume_usage: &volume_usage,
            compute_usage: &BTreeMap::new(),
            checkpoints: vec![],
            exports: vec![],
        })
    }

    #[test]
    fn the_report_always_names_the_host_side_port_and_omits_what_it_does_not_know() {
        let instance = to_reported_instance(&instance_record(|_| {}), None, None);
        let written = serde_json::to_value(&instance).unwrap();
        assert_eq!(written["hostPort"], u32::from(instance.host_port.unwrap()));
        for absent in [
            "startedAt",
            "lastHealthyAt",
            "lastExitCode",
            "message",
            "publicIpv4",
            "extraPublicPort",
            "compute",
        ] {
            assert!(written.get(absent).is_none(), "{absent} should be absent");
        }
        let exited = to_reported_instance(
            &instance_record(|record| record.last_exit_code = Some(0)),
            None,
            None,
        );
        assert_eq!(serde_json::to_value(&exited).unwrap()["lastExitCode"], 0);
    }

    /// The control plane holds neither half: the address is the relay's and the port is the
    /// slot's, so a report that omits them is an app nothing can be told where to reach.
    #[test]
    fn an_app_that_asked_for_its_own_port_is_reported_with_where_it_answers() {
        let reached = PublicAddress {
            ipv4: Ipv4Address::parse("203.0.113.7").unwrap(),
            port: HostPort::new(22_000).unwrap(),
        };
        let instance = to_reported_instance(
            &instance_record(|record| record.has_extra_public_port = Some(true)),
            Some(&reached),
            None,
        );
        assert_eq!(instance.public_ipv4, Some(reached.ipv4));
        assert_eq!(instance.extra_public_port, Some(reached.port));
    }

    /// Keyed on the app rather than on the volume, which is the mistake the two ids invite.
    #[test]
    fn a_volume_carries_the_reading_last_taken_of_it_and_no_other() {
        let matched = report_with([(app_id(), measured())].into_iter().collect());
        assert_eq!(matched.volumes[0].usage, Some(measured()));
        assert_eq!(report_with(BTreeMap::new()).volumes[0].usage, None);
        let other = AppId::parse("app-somebody-else").unwrap();
        assert_eq!(
            report_with([(other, measured())].into_iter().collect()).volumes[0].usage,
            None
        );
    }

    #[test]
    fn an_instance_carries_the_reading_last_taken_of_its_guest() {
        let spending = ComputeUsage {
            memory_total_bytes: 1_031_012_352,
            memory_used_bytes: 412_401_664,
            cpu_share: Some(0.18),
            measured_at: observed_at(),
        };
        let measured = to_reported_instance(&instance_record(|_| {}), None, Some(&spending));
        assert_eq!(measured.compute, Some(spending));
        assert_eq!(
            to_reported_instance(&instance_record(|_| {}), None, None).compute,
            None
        );
    }

    #[test]
    fn the_assembled_report_is_the_document_it_will_be_sent_as() {
        let report = report_with(BTreeMap::new());
        let written = serde_json::to_value(&report).unwrap();
        // Round-tripping through the wire types is the check: a field renamed on either side
        // stops matching here rather than on a host.
        let parsed: HostReportedState = serde_json::from_value(written).unwrap();
        assert_eq!(parsed, report);
    }
}
