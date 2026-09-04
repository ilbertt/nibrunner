use protocol::{
    AppHostname, AppId, DeploymentId, HealthCheck, HostPort, HttpPort, InstanceResources,
    InstanceState, Ipv4Address, Sha256Digest, StateMessage, Timestamp, VolumeId,
};
use serde::{Deserialize, Serialize};

use crate::backoff::{AttemptWindow, NO_START_ATTEMPTS};
use crate::health::{GraceInputs, HealthTracker};

/// A cache, not an authority: the microVM processes are what is actually running, and a record
/// that cannot be read back is discarded rather than trusted. Routing is rendered straight from
/// these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub volume_id: VolumeId,
    pub hostnames: Vec<AppHostname>,
    pub host_port: HostPort,
    pub http_port: HttpPort,
    /// Absent on a note written before an app could ask for one, which is every app that had not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_extra_public_port: Option<bool>,
    pub guest_ipv4: Ipv4Address,
    pub artifact_digest: Sha256Digest,
    pub state: InstanceState,
    pub health: HealthTracker,
    pub health_check: HealthCheck,
    pub resources: InstanceResources,
    pub desired_running: bool,
    /// Whether a request is what brings this app's microVM up. Beside `desired_running` rather
    /// than folded into it, because they answer different questions: should this app be
    /// reachable, and what does having it reachable cost while nobody is asking.
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

/// What desired state says about an app, as the record's own fields. Named once because a start
/// and a sleep write the same ones, and a field only one of them carried would be a record whose
/// contents depended on how the app happened to come to be here.
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

    /// The fields desired state owns, written over whatever the record held.
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

    /// Whether this app is asleep between requests: a record holding its slot and its hostnames
    /// with no microVM behind them.
    pub fn is_idle(&self) -> bool {
        self.state == InstanceState::Idle
    }

    pub fn wants_extra_public_port(&self) -> bool {
        self.has_extra_public_port.unwrap_or(false)
    }

    /// What every health decision about a record needs, in the one place that knows `started_at`
    /// is a wire timestamp here and a clock reading there.
    pub fn grace_inputs(&self, now_ms: i64) -> GraceInputs<'_> {
        GraceInputs {
            health_check: &self.health_check,
            started_at_ms: self.started_at.as_ref().map(protocol::Timestamp::epoch_ms),
            now_ms,
        }
    }
}

/// Whether a note this daemon left is one it can still read. Structural rather than
/// schema-driven: these are the daemon's own notes, and the recovery for an unreadable one is to
/// re-derive it from what is running rather than to reject the file.
pub fn read_instance_records(value: Option<serde_json::Value>) -> Vec<InstanceRecord> {
    let Some(serde_json::Value::Array(entries)) = value else {
        return Vec::new();
    };
    entries.into_iter().filter_map(|entry| serde_json::from_value(entry).ok()).collect()
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

    /// Dropping one is what replaces the still-running microVM it describes, so the guard stays
    /// the thing that decides.
    #[test]
    fn a_note_missing_a_field_this_daemon_needs_is_discarded_rather_than_guessed_at() {
        let mut written = serde_json::to_value(instance_record(|_| {})).unwrap();
        written.as_object_mut().unwrap().remove("httpPort");
        assert_eq!(read_instance_records(Some(serde_json::Value::Array(vec![written]))), vec![]);
        assert_eq!(read_instance_records(None), vec![]);
        assert_eq!(read_instance_records(Some(serde_json::json!({}))), vec![]);
    }

    /// The two fields a note written before they existed does not carry, and the defaults that
    /// let such a note still be read: an app that had not asked for a port, and a budget nothing
    /// had spent.
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
