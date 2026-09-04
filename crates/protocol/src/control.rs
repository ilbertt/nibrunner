//! What a host is told and what it answers with. There is deliberately nothing here shaped like
//! `start(x)` or `stop(x)`: the control plane describes a world and the host converges on it, so
//! a missed message, a host restart and a control-plane restart are all non-events.

use serde::{Deserialize, Serialize};

use crate::domain::*;
use crate::wire::*;

// ---------------------------------------------------------------------------------------------
// desired-state.ts

/// `on-request` is `running` with the microVM left out until something asks for it. A third
/// value rather than a flag beside these, because it is the same question: what should be true
/// of this app. A suspended app is `stopped` whatever its activation policy says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesiredInstanceState {
    Running,
    OnRequest,
    Stopped,
}

impl DesiredInstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            DesiredInstanceState::Running => "running",
            DesiredInstanceState::OnRequest => "on-request",
            DesiredInstanceState::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredPresence {
    Present,
    Absent,
}

/// `filename` is what the uploader called the binary, and it exists because `objectKey` cannot
/// answer that: keys are assigned to avoid collisions, so they carry no name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredArtifact {
    pub digest: Sha256Digest,
    pub size_bytes: u64,
    pub object_key: ObjectKey,
    pub filename: Filename,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredInstance {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub volume_id: VolumeId,
    pub desired_state: DesiredInstanceState,
    /// Only ever read for an `on-request` instance, and optional so an api that predates it
    /// leaves a host on its own default rather than stopping it converging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<IdleTimeoutMs>,
    pub artifact: DesiredArtifact,
    pub config: AppConfig,
    /// Carried down so the host can render its own routing config from the same state it boots
    /// VMs with, which is what leaves no room for the two to drift.
    pub hostnames: Vec<AppHostname>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredVolume {
    pub volume_id: VolumeId,
    pub app_id: AppId,
    pub size_bytes: u64,
    pub desired_state: DesiredPresence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredCheckpoint {
    pub checkpoint_id: CheckpointId,
    pub volume_id: VolumeId,
    pub desired_state: DesiredPresence,
}

/// A bundle the owning host should write, because it is the only party that can. `environment`
/// is optional and the distinction is the point: `{}` is an app that set no variables, and absent
/// is a control plane that cannot say which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredExport {
    pub export_id: ExportId,
    pub app_id: AppId,
    pub volume_id: VolumeId,
    pub object_key: ObjectKey,
    pub artifact: DesiredArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<TenantEnvironment>,
    pub desired_state: DesiredPresence,
}

/// The whole of what one host should be doing.
///
/// `instances` is authoritative: a microVM running on the host and absent from this list is one
/// the host stops and forgets. `volumes` and `checkpoints` are not: they hold tenant data, so
/// removing one is only ever expressed by an explicit `absent`, never implied by a list
/// shrinking. A truncated response must not be able to delete a filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDesiredState {
    pub host_id: HostId,
    pub volumes: Vec<DesiredVolume>,
    pub instances: Vec<DesiredInstance>,
    pub checkpoints: Vec<DesiredCheckpoint>,
    pub exports: Vec<DesiredExport>,
}

// ---------------------------------------------------------------------------------------------
// reported-state.ts

pub const MAX_DEVICE_PATH_LENGTH: usize = 256;

/// Optional fields are omitted rather than sent empty: absent is the one convention for unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedInstance {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub state: InstanceState,
    /// Reported even though routing is local to the host: the control plane needs it to debug a
    /// host it cannot connect to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<HostPort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_ipv4: Option<Ipv4Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ipv4: Option<Ipv4Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_public_port: Option<HostPort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<Sha256Digest>,
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_healthy_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StateMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedVolume {
    pub volume_id: VolumeId,
    pub app_id: AppId,
    pub state: VolumeState,
    pub size_bytes: u64,
    /// Which storage prefix the host put it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_prefix: Option<ObjectKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<FilesystemUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StateMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedCheckpoint {
    pub checkpoint_id: CheckpointId,
    pub volume_id: VolumeId,
    pub state: CheckpointState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<StateMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StateMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedExport {
    pub export_id: ExportId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<CheckpointId>,
    pub state: ExportState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StateMessage>,
}

/// What a host is actually doing, as observed by the daemon, not as it remembers having
/// arranged. On startup the daemon enumerates what is really running before it reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostReportedState {
    pub host_id: HostId,
    pub reported_at: Timestamp,
    pub state: HostState,
    pub capacity: HostCapacity,
    pub allocatable: HostCapacity,
    pub versions: HostVersions,
    pub volumes: Vec<ReportedVolume>,
    pub instances: Vec<ReportedInstance>,
    pub checkpoints: Vec<ReportedCheckpoint>,
    pub exports: Vec<ReportedExport>,
}

// ---------------------------------------------------------------------------------------------
// session.ts

pub const MIN_POLL_INTERVAL_MS: u64 = 100;

/// How often the host should come back. The control plane sets these rather than the host, so
/// a fleet can be backed off without redeploying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPollSettings {
    pub min_interval_ms: u64,
    pub report_interval_ms: u64,
}

/// A quarter of a second, because this is what a deploy waits out before anything has begun.
pub const DEFAULT_AGENT_POLL_SETTINGS: AgentPollSettings = AgentPollSettings {
    min_interval_ms: 250,
    report_interval_ms: 15_000,
};

/// Opens a session. There is deliberately no credential here: the internal port is reachable
/// from inside the VPC and nowhere else, and a tenant microVM is denied it by the host's own
/// ruleset before it can route anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRequest {
    /// Absent on a host's very first registration; the control plane assigns one and the host
    /// persists it, so a reinstalled host rejoins as the same host rather than as a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<HostId>,
    pub versions: HostVersions,
    pub capacity: HostCapacity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSession {
    pub host_id: HostId,
    pub session_token: SecretString,
    pub expires_at: Timestamp,
    pub poll: AgentPollSettings,
}

// ---------------------------------------------------------------------------------------------
// transport.ts

/// The protocol's own version, sent on every request. Still 1 while the first version is being
/// shaped: it starts moving when something not deployed from the nibrun repo depends on the shape.
pub const PROTOCOL_VERSION: u32 = 1;
pub const PROTOCOL_VERSION_HEADER: &str = "x-nibrun-protocol-version";

/// Under /internal, not /api: the public edge answers 404 for that whole namespace.
pub const AGENT_API_PREFIX: &str = "/internal/agent";

/// Every route is an outbound POST carrying one JSON document, including the two that read.
pub mod agent_routes {
    pub const SESSION: &str = "/session";
    pub const DESIRED_STATE: &str = "/desired-state";
    pub const REPORTED_STATE: &str = "/reported-state";
    pub const FILESYSTEM_QUERY: &str = "/filesystem-query";
    pub const FILESYSTEM_QUERY_RESULT: &str = "/filesystem-query-result";
}

/// Nothing, and deliberately: a host asking what it should be running has nothing to tell the
/// control plane to get an answer. The host holds the last state anyway, so it can see for itself
/// whether the one that arrived differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DesiredStateRequest {}

pub type DesiredStateResponse = HostDesiredState;

// ---------------------------------------------------------------------------------------------
// filesystem-query.ts

pub const MAX_QUERY_MESSAGE_LENGTH: usize = 512;
pub const MAX_SERVED_APPS: usize = 200;

/// What a host is willing to answer, restated on every poll rather than remembered by the
/// control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemQueryRequest {
    pub served_app_ids: Vec<AppId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemQuery {
    pub query_id: FilesystemQueryId,
    pub app_id: AppId,
    pub path: GuestPath,
}

/// `none` is the common answer and carries nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "lowercase")]
pub enum FilesystemQueryResponse {
    None,
    Query { query: FilesystemQuery },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum FilesystemQueryOutcome {
    Listed { listing: DirectoryListing },
    Failed { message: String },
}

/// Answered exactly once per `queryId`, including when it could not be answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemQueryResult {
    pub query_id: FilesystemQueryId,
    pub outcome: FilesystemQueryOutcome,
}
