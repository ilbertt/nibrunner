use serde::{Deserialize, Serialize};

use crate::domain::*;
use crate::wire::*;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredArtifact {
    pub digest: Sha256Digest,
    pub size_bytes: u64,
    pub object_key: ObjectKey,
    pub filename: Filename,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "DesiredInstanceFields")]
pub struct DesiredInstance {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub volume_id: VolumeId,
    pub desired_state: DesiredInstanceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<IdleTimeoutMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<ActivationPolicy>,
    pub artifact: DesiredArtifact,
    pub config: AppConfig,
    pub hostnames: Vec<AppHostname>,
}

impl DesiredInstance {
    /// The policy this instance runs under, whichever of the two spellings its document used.
    /// A document that used neither is one written before either existed, and means what it
    /// meant then.
    pub fn activation(&self) -> ActivationPolicy {
        self.activation.unwrap_or_else(|| ActivationPolicy {
            sleep_when: match self.desired_state {
                DesiredInstanceState::OnRequest => SleepPolicy::TrafficIdle {
                    timeout_ms: self.idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT),
                },
                DesiredInstanceState::Running | DesiredInstanceState::Stopped => SleepPolicy::Never,
            },
            ready_when: ReadinessPolicy::PortAnswers,
        })
    }
}

// The document is read once and refused whole, so a pair of fields that disagree about when an
// instance sleeps is caught here rather than by whichever loop read one of them first.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesiredInstanceFields {
    app_id: AppId,
    deployment_id: DeploymentId,
    volume_id: VolumeId,
    desired_state: DesiredInstanceState,
    #[serde(default)]
    idle_timeout_ms: Option<IdleTimeoutMs>,
    #[serde(default)]
    activation: Option<ActivationPolicy>,
    artifact: DesiredArtifact,
    config: AppConfig,
    hostnames: Vec<AppHostname>,
}

impl TryFrom<DesiredInstanceFields> for DesiredInstance {
    type Error = InvalidValue;

    fn try_from(fields: DesiredInstanceFields) -> Result<Self, Self::Error> {
        if fields.activation.is_some() && fields.idle_timeout_ms.is_some() {
            return Err(InvalidValue::new_public(
                "activation and idleTimeoutMs both say when an instance sleeps; name one",
            ));
        }
        if let Some(policy) = &fields.activation {
            if fields.desired_state != DesiredInstanceState::OnRequest
                && policy.sleep_when != SleepPolicy::Never
            {
                return Err(InvalidValue::new_public(
                    "only an on-request instance may name a sleepWhen other than never, because nothing would wake it again",
                ));
            }
        }
        Ok(Self {
            app_id: fields.app_id,
            deployment_id: fields.deployment_id,
            volume_id: fields.volume_id,
            desired_state: fields.desired_state,
            idle_timeout_ms: fields.idle_timeout_ms,
            activation: fields.activation,
            artifact: fields.artifact,
            config: fields.config,
            hostnames: fields.hostnames,
        })
    }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDesiredState {
    pub host_id: HostId,
    pub volumes: Vec<DesiredVolume>,
    pub instances: Vec<DesiredInstance>,
    pub checkpoints: Vec<DesiredCheckpoint>,
    pub exports: Vec<DesiredExport>,
}

pub const MAX_DEVICE_PATH_LENGTH: usize = 256;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedInstance {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub state: InstanceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<HostPort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_ipv4: Option<Ipv4Address>,
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

pub const MIN_POLL_INTERVAL_MS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPollSettings {
    pub min_interval_ms: u64,
    pub report_interval_ms: u64,
}

pub const DEFAULT_AGENT_POLL_SETTINGS: AgentPollSettings = AgentPollSettings {
    min_interval_ms: 250,
    report_interval_ms: 15_000,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRequest {
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

pub const PROTOCOL_VERSION: u32 = 1;
pub const PROTOCOL_VERSION_HEADER: &str = "x-nibrun-protocol-version";

pub const AGENT_API_PREFIX: &str = "/internal/agent";

pub mod agent_routes {
    pub const SESSION: &str = "/session";
    pub const DESIRED_STATE: &str = "/desired-state";
    pub const REPORTED_STATE: &str = "/reported-state";
    pub const FILESYSTEM_QUERY: &str = "/filesystem-query";
    pub const FILESYSTEM_QUERY_RESULT: &str = "/filesystem-query-result";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DesiredStateRequest {}

pub type DesiredStateResponse = HostDesiredState;

pub const MAX_QUERY_MESSAGE_LENGTH: usize = 512;
pub const MAX_SERVED_APPS: usize = 200;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemQueryResult {
    pub query_id: FilesystemQueryId,
    pub outcome: FilesystemQueryOutcome,
}
