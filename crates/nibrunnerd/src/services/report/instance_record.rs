use protocol::{
    AppHostname, AppId, DeploymentId, HealthCheck, HostPort, HttpPort, InstanceResources, InstanceState,
    Ipv4Address, Sha256Digest, StateMessage, Timestamp, VolumeId,
};
use serde::{Deserialize, Serialize};

use crate::services::backoff::{AttemptWindow, NO_START_ATTEMPTS};
use crate::services::health::{GraceInputs, HealthTracker};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub volume_id: VolumeId,
    pub hostnames: Vec<AppHostname>,
    pub host_port: HostPort,
    pub http_port: HttpPort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_extra_public_port: Option<bool>,
    pub guest_ipv4: Ipv4Address,
    pub artifact_digest: Sha256Digest,
    pub state: InstanceState,
    pub health: HealthTracker,
    pub health_check: HealthCheck,
    pub resources: InstanceResources,
    pub desired_running: bool,
    pub on_request: bool,
    #[serde(default)]
    pub start_attempts: AttemptWindow,
    pub restart_count: u32,
    #[serde(default)]
    pub stop_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StateMessage>,
}

#[derive(Debug, Clone)]
pub struct RecordFields {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub volume_id: VolumeId,
    pub hostnames: Vec<AppHostname>,
    pub host_port: HostPort,
    pub http_port: HttpPort,
    pub has_extra_public_port: Option<bool>,
    pub guest_ipv4: Ipv4Address,
    pub artifact_digest: Sha256Digest,
    pub health_check: HealthCheck,
    pub resources: InstanceResources,
    pub desired_running: bool,
    pub on_request: bool,
}

impl InstanceRecord {
    pub fn new(fields: RecordFields, state: InstanceState, health: HealthTracker) -> Self {
        Self {
            app_id: fields.app_id,
            deployment_id: fields.deployment_id,
            volume_id: fields.volume_id,
            hostnames: fields.hostnames,
            host_port: fields.host_port,
            http_port: fields.http_port,
            has_extra_public_port: fields.has_extra_public_port,
            guest_ipv4: fields.guest_ipv4,
            artifact_digest: fields.artifact_digest,
            state,
            health,
            health_check: fields.health_check,
            resources: fields.resources,
            desired_running: fields.desired_running,
            on_request: fields.on_request,
            start_attempts: NO_START_ATTEMPTS,
            restart_count: 0,
            stop_requested: false,
            started_at: None,
            last_exit_code: None,
            message: None,
        }
    }

    pub fn adopt(&mut self, fields: RecordFields) {
        self.deployment_id = fields.deployment_id;
        self.volume_id = fields.volume_id;
        self.hostnames = fields.hostnames;
        self.host_port = fields.host_port;
        self.http_port = fields.http_port;
        self.has_extra_public_port = fields.has_extra_public_port;
        self.guest_ipv4 = fields.guest_ipv4;
        self.artifact_digest = fields.artifact_digest;
        self.health_check = fields.health_check;
        self.resources = fields.resources;
        self.desired_running = fields.desired_running;
        self.on_request = fields.on_request;
    }

    pub fn is_idle(&self) -> bool {
        self.state == InstanceState::Idle
    }

    pub fn wants_extra_public_port(&self) -> bool {
        self.has_extra_public_port.unwrap_or(false)
    }

    pub fn grace_inputs(&self, now_ms: i64) -> GraceInputs<'_> {
        GraceInputs {
            health_check: &self.health_check,
            started_at_ms: self.started_at.as_ref().map(protocol::Timestamp::epoch_ms),
            now_ms,
        }
    }
}

pub fn read_instance_records(value: Option<serde_json::Value>) -> Vec<InstanceRecord> {
    let Some(serde_json::Value::Array(entries)) = value else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::instance_record;

    #[test]
    fn a_record_round_trips_through_the_notes_this_daemon_writes() {
        let record = instance_record(|_| {});
        let written = serde_json::to_value(vec![record.clone()]).unwrap();
        assert_eq!(read_instance_records(Some(written)), vec![record]);
    }

    #[test]
    fn a_note_missing_a_field_this_daemon_needs_is_discarded_rather_than_guessed_at() {
        let mut written = serde_json::to_value(instance_record(|_| {})).unwrap();
        written.as_object_mut().unwrap().remove("httpPort");
        assert_eq!(
            read_instance_records(Some(serde_json::Value::Array(vec![written]))),
            vec![]
        );
        assert_eq!(read_instance_records(None), vec![]);
        assert_eq!(read_instance_records(Some(serde_json::json!({}))), vec![]);
    }

    #[test]
    fn a_note_that_predates_a_field_reads_as_the_no_it_meant() {
        let mut written = serde_json::to_value(instance_record(|_| {})).unwrap();
        let object = written.as_object_mut().unwrap();
        object.remove("hasExtraPublicPort");
        object.remove("startAttempts");
        let records = read_instance_records(Some(serde_json::Value::Array(vec![written])));
        assert_eq!(records.len(), 1);
        assert!(!records[0].wants_extra_public_port());
        assert_eq!(records[0].start_attempts, NO_START_ATTEMPTS);
    }
}
