use serde::{Deserialize, Serialize};

use crate::domain::*;
use crate::wire::*;

/// The whole of an instance's activation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum DesiredInstanceState {
    /// Keeps the microVM up.
    Running,
    /// Brings the microVM up for the first deploy and for every request that finds it asleep, and
    /// lets it sleep again once it has been quiet for `idleTimeoutMs`.
    OnRequest,
    /// Takes the microVM down and leaves the app reachable enough to say so.
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum DesiredPresence {
    Present,
    Absent,
}

pub const MAX_LAYERS: usize = 8;

/// An object in the store the host's `artifacts.store_url` names, checked against `digest`
/// before anything boots from it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct LayerObject {
    pub digest: Sha256Digest,
    pub size_bytes: u64,
    /// Where the object lives in the store.
    pub object_key: ObjectKey,
}

/// One read-only layer of the root filesystem an instance boots into. Layers stack in the order
/// the document lists them, first at the bottom, and the app's volume is stacked writable over
/// all of them. The kind says what the object is, and so what the host does with it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum DesiredLayer {
    /// A squashfs or ext4 image, attached as it was uploaded.
    Filesystem {
        #[serde(flatten)]
        object: LayerObject,
    },
    /// One program, packed into an image at `destinationPath` and run the way this host has
    /// always run one: by the guest's own init, with the app's arguments and environment.
    Executable {
        #[serde(flatten)]
        object: LayerObject,
        destination_path: GuestPath,
    },
}

impl DesiredLayer {
    pub fn object(&self) -> &LayerObject {
        match self {
            DesiredLayer::Filesystem { object } | DesiredLayer::Executable { object, .. } => object,
        }
    }
}

/// Where the guest's init lives in a stacked root, and so the one place a program cannot be put.
pub const INIT_PATH: &str = "/sbin/init";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(!try_from, transform = desired_instance_rules))]
#[serde(rename_all = "camelCase", try_from = "DesiredInstanceFields")]
pub struct DesiredInstance {
    pub app_id: AppId,
    /// A running instance is replaced when this changes, and only then: a new layer or config
    /// under the same `deploymentId` is not picked up.
    pub deployment_id: DeploymentId,
    /// One of this document's `volumes`, mounted in the guest as the app's data directory.
    pub volume_id: VolumeId,
    pub desired_state: DesiredInstanceState,
    /// How long an `on-request` instance stays up after its last request before it sleeps: the
    /// older spelling of `activation.sleepWhen`, refused beside it. 300000 when neither is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<IdleTimeoutMs>,
    /// What puts this instance to sleep and what tells the host it is ready. A `sleepWhen` other
    /// than `never` is refused on anything but an `on-request` instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<ActivationPolicy>,
    /// The root filesystem, bottom layer first. At least one; at most `MAX_LAYERS`.
    pub layers: Vec<DesiredLayer>,
    pub config: AppConfig,
    /// What the HTTP proxy routes to this app's `httpPort`. Empty for an app nothing outside needs
    /// to reach by name.
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
    layers: Vec<DesiredLayer>,
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
        if fields.layers.is_empty() {
            return Err(InvalidValue::new_public(
                "an instance names at least one layer, because a microVM boots from something",
            ));
        }
        if fields.layers.len() > MAX_LAYERS {
            return Err(InvalidValue::new_public(&format!(
                "an instance names at most {MAX_LAYERS} layers, because a microVM has that many drives to give them"
            )));
        }
        for layer in &fields.layers {
            let DesiredLayer::Executable { destination_path, .. } = layer else {
                continue;
            };
            if destination_path.as_str() == "/" {
                return Err(InvalidValue::new_public(
                    "an executable's destinationPath names the file the program becomes, and / is not a file",
                ));
            }
            if destination_path.as_str() == INIT_PATH {
                return Err(InvalidValue::new_public(&format!(
                    "an executable cannot be put at {INIT_PATH}, which is what starts it"
                )));
            }
        }
        Ok(Self {
            app_id: fields.app_id,
            deployment_id: fields.deployment_id,
            volume_id: fields.volume_id,
            desired_state: fields.desired_state,
            idle_timeout_ms: fields.idle_timeout_ms,
            activation: fields.activation,
            layers: fields.layers,
            config: fields.config,
            hostnames: fields.hostnames,
        })
    }
}

// What `TryFrom<DesiredInstanceFields>` refuses, said in the schema's words so a document an editor
// passes is one this host takes.
#[cfg(feature = "schema")]
fn desired_instance_rules(schema: &mut schemars::Schema) {
    schema.insert(
        "not".into(),
        serde_json::json!({ "required": ["activation", "idleTimeoutMs"] }),
    );
    schema.insert(
        "if".into(),
        serde_json::json!({
            "required": ["activation"],
            "properties": { "desiredState": { "not": { "const": "on-request" } } }
        }),
    );
    schema.insert(
        "then".into(),
        serde_json::json!({
            "properties": {
                "activation": { "properties": { "sleepWhen": { "properties": { "kind": { "const": "never" } } } } }
            }
        }),
    );
    if let Some(layers) = schema
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("layers"))
        .and_then(serde_json::Value::as_object_mut)
    {
        layers.insert("minItems".into(), serde_json::json!(1));
        layers.insert("maxItems".into(), serde_json::json!(MAX_LAYERS));
        layers.insert(
            "items".into(),
            serde_json::json!({
                "allOf": [
                    layers.get("items").cloned().unwrap_or(serde_json::json!({})),
                    { "properties": { "destinationPath": { "not": { "enum": ["/", INIT_PATH] } } } }
                ]
            }),
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DesiredVolume {
    pub volume_id: VolumeId,
    pub app_id: AppId,
    pub size_bytes: u64,
    pub desired_state: DesiredPresence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DesiredCheckpoint {
    pub checkpoint_id: CheckpointId,
    pub volume_id: VolumeId,
    pub desired_state: DesiredPresence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DesiredExport {
    pub export_id: ExportId,
    pub app_id: AppId,
    pub volume_id: VolumeId,
    pub object_key: ObjectKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<TenantEnvironment>,
    pub desired_state: DesiredPresence,
}

/// What one host should be running. The daemon watches this document at the path its
/// `paths.desired_state_file` names and converges on every change to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ReportedInstance {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub state: InstanceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<HostPort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_ipv4: Option<Ipv4Address>,
    /// The layers the running microVM was booted from, bottom first. Empty until one has been.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layer_digests: Vec<Sha256Digest>,
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_healthy_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeUsage>,
    // Not optional the way `compute` is: a guest that never answered has no usage to report, but
    // an app this host has only ever held a record of has used nothing, and nothing is a number.
    #[serde(default)]
    pub meters: UsageMeters,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StateMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// What one host is running, as the daemon last wrote it to `reported.json` in its
/// `paths.state_dir`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct HostReportedState {
    pub host_id: HostId,
    pub reported_at: Timestamp,
    pub state: HostState,
    /// What the machine has.
    pub capacity: HostCapacity,
    /// What is left once every booted app is taken off.
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
