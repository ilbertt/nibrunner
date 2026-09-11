use std::collections::BTreeMap;

use protocol::{
    AppId, ComputeUsage, FilesystemUsage, HostCapacity, HostId, HostReportedState, HostState, HostVersions,
    ReportedCheckpoint, ReportedExport, ReportedInstance, ReportedVolume, Timestamp, UsageMeters,
};

use crate::domain::report::InstanceRecord;

pub fn to_reported_instance(
    record: &InstanceRecord,
    measured: Option<&ComputeUsage>,
    metered: Option<&UsageMeters>,
) -> ReportedInstance {
    ReportedInstance {
        app_id: record.app_id.clone(),
        deployment_id: record.deployment_id.clone(),
        state: record.state,
        host_port: Some(record.host_port),
        guest_ipv4: Some(record.guest_ipv4.clone()),
        artifact_digest: Some(record.artifact_digest.clone()),
        restart_count: record.restart_count,
        started_at: record.started_at.clone(),
        last_healthy_at: record.health.last_healthy_at.clone(),
        last_exit_code: record.last_exit_code,
        compute: measured.cloned(),
        meters: metered.copied().unwrap_or_default(),
        message: record.message.clone(),
    }
}

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
    pub volumes: Vec<ReportedVolume>,
    pub volume_usage: &'a BTreeMap<AppId, FilesystemUsage>,
    pub compute_usage: &'a BTreeMap<AppId, ComputeUsage>,
    pub meters: &'a BTreeMap<AppId, UsageMeters>,
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
                    inputs.compute_usage.get(&record.app_id),
                    inputs.meters.get(&record.app_id),
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
    use protocol::VolumeState;

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

    fn assembled(
        records: &[InstanceRecord],
        checkpoints: Vec<ReportedCheckpoint>,
        exports: Vec<ReportedExport>,
    ) -> HostReportedState {
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
            records,
            volumes: vec![],
            volume_usage: &BTreeMap::new(),
            compute_usage: &BTreeMap::new(),
            meters: &BTreeMap::new(),
            checkpoints,
            exports,
        })
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
            volumes: vec![reported_volume()],
            volume_usage: &volume_usage,
            compute_usage: &BTreeMap::new(),
            meters: &BTreeMap::new(),
            checkpoints: vec![],
            exports: vec![],
        })
    }

    #[test]
    fn the_report_always_names_the_host_side_port_and_omits_what_it_does_not_know() {
        let instance = to_reported_instance(&instance_record(|_| {}), None, None);
        let written = serde_json::to_value(&instance).unwrap();
        assert_eq!(written["hostPort"], u32::from(instance.host_port.unwrap()));
        for absent in ["startedAt", "lastHealthyAt", "lastExitCode", "message", "compute"] {
            assert!(written.get(absent).is_none(), "{absent} should be absent");
        }
        let exited = to_reported_instance(
            &instance_record(|record| record.last_exit_code = Some(0)),
            None,
            None,
        );
        assert_eq!(serde_json::to_value(&exited).unwrap()["lastExitCode"], 0);
    }

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
        let measured = to_reported_instance(&instance_record(|_| {}), Some(&spending), None);
        assert_eq!(measured.compute, Some(spending));
        assert_eq!(
            to_reported_instance(&instance_record(|_| {}), None, None).compute,
            None
        );
    }

    #[test]
    fn an_instance_carries_what_it_has_used_rather_than_what_it_was_last_seen_using() {
        let metered = protocol::UsageMeters {
            running_ms: 3_600_000,
            idle_ms: 900_000,
            cpu_ms: 42_150,
            rx_bytes: 1_073_741_824,
            tx_bytes: 4_294_967_296,
            disk_provisioned_mib_seconds: 29_491_200,
            disk_used_mib_seconds: 5_242_880,
        };
        let reported = to_reported_instance(&instance_record(|_| {}), None, Some(&metered));
        assert_eq!(reported.meters, metered);

        let never = to_reported_instance(&instance_record(|_| {}), None, None);
        assert_eq!(
            never.meters,
            protocol::UsageMeters::default(),
            "an app nothing has been metered about has used nothing"
        );
    }

    #[test]
    fn every_record_the_host_holds_reaches_the_report_in_the_order_it_was_given() {
        let records: Vec<InstanceRecord> = ["app-one", "app-two", "app-three"]
            .iter()
            .map(|name| instance_record(|record| record.app_id = AppId::parse(*name).unwrap()))
            .collect();
        let report = assembled(&records, vec![], vec![]);
        assert_eq!(
            report
                .instances
                .iter()
                .map(|instance| instance.app_id.as_str().to_string())
                .collect::<Vec<_>>(),
            vec![
                "app-one".to_string(),
                "app-two".to_string(),
                "app-three".to_string()
            ]
        );
        assert!(assembled(&[], vec![], vec![]).instances.is_empty());
    }

    #[test]
    fn what_the_host_observed_of_its_checkpoints_and_exports_is_passed_on_untouched() {
        let checkpoint = ReportedCheckpoint {
            checkpoint_id: checkpoint_id(),
            volume_id: volume_id(),
            state: protocol::CheckpointState::Ready,
            reference: None,
            ready_at: Some(observed_at()),
            message: None,
        };
        let export = ReportedExport {
            export_id: export_id(),
            checkpoint_id: Some(checkpoint_id()),
            state: protocol::ExportState::Failed,
            size_bytes: None,
            ready_at: None,
            message: Some(protocol::StateMessage::new(
                "the volume would not freeze".to_string(),
            )),
        };
        let report = assembled(&[], vec![checkpoint.clone()], vec![export.clone()]);
        assert_eq!(report.checkpoints, vec![checkpoint]);
        assert_eq!(report.exports, vec![export]);
    }

    #[test]
    fn the_assembled_report_is_the_document_it_will_be_sent_as() {
        let report = report_with(BTreeMap::new());
        let written = serde_json::to_value(&report).unwrap();
        let parsed: HostReportedState = serde_json::from_value(written).unwrap();
        assert_eq!(parsed, report);
    }
}
