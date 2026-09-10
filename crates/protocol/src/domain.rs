use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wire::*;

const ENVIRONMENT_RESERVED_NAME: &str = "__proto__";

pub fn is_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    name != ENVIRONMENT_RESERVED_NAME
        && matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

const RUNTIME_VALUE_PREFIX: &str = "NIBRUN_";

pub const RUNTIME_VALUE_NAMES: [&str; 3] = ["NIBRUN_DATA_DIR", "NIBRUN_HOSTNAME", "NIBRUN_HTTP_PORT"];

fn is_name_character(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn runtime_references(value: &str) -> Vec<(String, bool)> {
    let mut found = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        let braced = bytes.get(cursor) == Some(&b'{');
        if braced {
            cursor += 1;
        }
        if !value[cursor..].starts_with(RUNTIME_VALUE_PREFIX) {
            index += 1;
            continue;
        }
        let start = cursor;
        while cursor < bytes.len() && is_name_character(bytes[cursor] as char) {
            cursor += 1;
        }
        let name = &value[start..cursor];
        let closed = if braced {
            let closed = bytes.get(cursor) == Some(&b'}');
            if closed {
                cursor += 1;
            }
            closed
        } else {
            true
        };
        found.push((name.to_string(), closed && RUNTIME_VALUE_NAMES.contains(&name)));
        index = cursor.max(index + 1);
    }
    found
}

pub fn names_offered_runtime_values(value: &str) -> bool {
    runtime_references(value).iter().all(|(_, allowed)| *allowed)
}

pub fn interpolable_runtime_value(name: &str) -> String {
    format!("${{{name}}}")
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SecretString", into = "SecretString")]
pub struct TenantValue(SecretString);

impl TenantValue {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidValue> {
        let secret = SecretString::parse(value)?;
        if !names_offered_runtime_values(secret.expose()) {
            return Err(InvalidValue::new_public(
                "a tenant value names a runtime value the guest does not offer",
            ));
        }
        Ok(Self(secret))
    }

    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl TryFrom<SecretString> for TenantValue {
    type Error = InvalidValue;
    fn try_from(value: SecretString) -> Result<Self, Self::Error> {
        Self::parse(value.expose())
    }
}

impl From<TenantValue> for SecretString {
    fn from(value: TenantValue) -> SecretString {
        value.0
    }
}

impl std::fmt::Debug for TenantValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<String, TenantValue>",
    into = "BTreeMap<String, TenantValue>"
)]
pub struct TenantEnvironment(BTreeMap<String, TenantValue>);

impl TenantEnvironment {
    pub fn iter(&self) -> impl Iterator<Item = (&str, &TenantValue)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl TryFrom<BTreeMap<String, TenantValue>> for TenantEnvironment {
    type Error = InvalidValue;
    fn try_from(value: BTreeMap<String, TenantValue>) -> Result<Self, Self::Error> {
        if let Some(name) = value.keys().find(|name| !is_environment_name(name)) {
            return Err(InvalidValue::new_public(&format!(
                "{name} is not an environment variable name"
            )));
        }
        Ok(Self(value))
    }
}

impl From<TenantEnvironment> for BTreeMap<String, TenantValue> {
    fn from(value: TenantEnvironment) -> Self {
        value.0
    }
}

impl std::fmt::Debug for TenantEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|name| (name, REDACTED)))
            .finish()
    }
}

impl FromIterator<(String, TenantValue)> for TenantEnvironment {
    fn from_iter<T: IntoIterator<Item = (String, TenantValue)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

pub const MAX_ARGUMENTS: usize = 64;
pub const MAX_ARGUMENT_LENGTH: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct TenantArguments(Vec<String>);

impl TenantArguments {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Vec<String>> for TenantArguments {
    type Error = InvalidValue;
    fn try_from(value: Vec<String>) -> Result<Self, Self::Error> {
        if value.len() > MAX_ARGUMENTS {
            return Err(InvalidValue::new_public("too many arguments"));
        }
        if value.iter().any(|argument| argument.len() > MAX_ARGUMENT_LENGTH) {
            return Err(InvalidValue::new_public("an argument is too long"));
        }
        Ok(Self(value))
    }
}

impl From<TenantArguments> for Vec<String> {
    fn from(value: TenantArguments) -> Self {
        value.0
    }
}

pub const MIN_HOSTNAMES: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppHostnameKind {
    Platform,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppHostname {
    pub hostname: Hostname,
    pub kind: AppHostnameKind,
}

/// How the host puts a guest port within reach of the world.
///
/// `tcp` is a byte pipe and nothing more: the host reads none of what crosses it, which is what
/// lets a protocol this host does not speak — ssh among them — arrive at all. The HTTP port is
/// not one of these, because the proxy has to read a request to know whose hostname it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortIngress {
    Tcp,
}

/// A port an app answers on beyond its HTTP one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstancePort {
    pub name: PortName,
    pub guest_port: GuestPort,
    pub ingress: PortIngress,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub http_port: HttpPort,
    /// What this app answers on besides `http_port`. Absent is the shape every document had
    /// before there was anything to put here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<InstancePort>,
    pub args: TenantArguments,
    pub environment: TenantEnvironment,
    pub resources: InstanceResources,
    pub health_check: HealthCheck,
    pub restart_policy: RestartPolicy,
}

/// How many ports an app may name is the host's to say — `proxy.tcp.ports_per_app` — so what is
/// wrong here is only ever the shape of the list, never its length.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortsInvalid {
    #[error("{name} is named twice, and each port is reached by its own name")]
    DuplicateName { name: String },
    #[error("guest port {port} is claimed twice, and one port cannot answer two ways")]
    DuplicateGuestPort { port: u16 },
}

impl AppConfig {
    /// Every port this app answers on, the HTTP one first.
    ///
    /// The order is the one the slot's host ports are handed out in, so an app that keeps its
    /// list keeps its ports across a restart.
    pub fn all_ports(&self) -> Vec<InstancePort> {
        let http = InstancePort {
            name: PortName::parse(HTTP_PORT_NAME).expect("a constant this crate wrote"),
            guest_port: GuestPort::new(self.http_port.get()).expect("a port is never zero"),
            ingress: PortIngress::Tcp,
        };
        std::iter::once(http).chain(self.ports.iter().cloned()).collect()
    }

    pub fn validate_ports(&self) -> Result<(), PortsInvalid> {
        let mut names = std::collections::BTreeSet::new();
        let mut guest_ports = std::collections::BTreeSet::new();
        for port in self.all_ports() {
            if !names.insert(port.name.as_str().to_string()) {
                return Err(PortsInvalid::DuplicateName {
                    name: port.name.as_str().to_string(),
                });
            }
            if !guest_ports.insert(port.guest_port.get()) {
                return Err(PortsInvalid::DuplicateGuestPort {
                    port: port.guest_port.get(),
                });
            }
        }
        Ok(())
    }
}

/// What the HTTP port is called wherever ports are named together.
pub const HTTP_PORT_NAME: &str = "http";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppActivation {
    Always,
    OnRequest,
}

pub const MIN_IDLE_TIMEOUT_MS: u64 = 60_000;
pub const MAX_IDLE_TIMEOUT_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct IdleTimeoutMs(u64);

impl IdleTimeoutMs {
    pub fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for IdleTimeoutMs {
    type Error = InvalidValue;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (MIN_IDLE_TIMEOUT_MS..=MAX_IDLE_TIMEOUT_MS).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidValue::new_public("idleTimeoutMs is out of range"))
        }
    }
}

impl From<IdleTimeoutMs> for u64 {
    fn from(value: IdleTimeoutMs) -> u64 {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppState {
    Active,
    Suspended,
    Deleting,
    Deleted,
}

pub const MIN_VCPU_COUNT: u32 = 1;
pub const MAX_VCPU_COUNT: u32 = 32;
pub const MIN_MEMORY_MIB: u32 = 128;
pub const MAX_MEMORY_MIB: u32 = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceResources {
    pub vcpu_count: u32,
    pub memory_mib: u32,
}

pub const DEFAULT_INSTANCE_RESOURCES: InstanceResources = InstanceResources {
    vcpu_count: 1,
    memory_mib: 256,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub grace_period_ms: u64,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
}

pub const DEFAULT_HEALTH_CHECK: HealthCheck = HealthCheck {
    path: None,
    interval_ms: 5_000,
    timeout_ms: 2_000,
    grace_period_ms: 30_000,
    healthy_threshold: 1,
    unhealthy_threshold: 3,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_factor: f64,
    pub reset_after_ms: u64,
}

pub const DEFAULT_RESTART_POLICY: RestartPolicy = RestartPolicy {
    max_restarts: 5,
    initial_backoff_ms: 500,
    max_backoff_ms: 30_000,
    backoff_factor: 2.0,
    reset_after_ms: 60_000,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Pending,
    Starting,
    Running,
    Unhealthy,
    Stopping,
    Stopped,
    Idle,
    Failed,
}

pub const INSTANCE_STATES: [InstanceState; 8] = [
    InstanceState::Pending,
    InstanceState::Starting,
    InstanceState::Running,
    InstanceState::Unhealthy,
    InstanceState::Stopping,
    InstanceState::Stopped,
    InstanceState::Idle,
    InstanceState::Failed,
];

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Pending => "pending",
            InstanceState::Starting => "starting",
            InstanceState::Running => "running",
            InstanceState::Unhealthy => "unhealthy",
            InstanceState::Stopping => "stopping",
            InstanceState::Stopped => "stopped",
            InstanceState::Idle => "idle",
            InstanceState::Failed => "failed",
        }
    }
}

pub const DEFAULT_VOLUME_SIZE_BYTES: u64 = 8_589_934_592;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeState {
    Pending,
    Ready,
    Detached,
    Deleted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckpointState {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportState {
    Pending,
    Preparing,
    Ready,
    Failed,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostVersions {
    pub agent: String,
    pub guest_image: String,
    pub zerofs: String,
    pub firecracker: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostCapacity {
    pub vcpu_count: u32,
    pub memory_mib: u64,
    pub cache_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostState {
    Registering,
    Ready,
    Draining,
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeUsage {
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_share: Option<f64>,
    pub measured_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilesystemEntryKind {
    File,
    Directory,
    Other,
}

pub const MAX_ENTRY_NAME_LENGTH: usize = 255;

pub fn is_entry_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_ENTRY_NAME_LENGTH && !name.contains(['/', '\0'])
}

pub const MAX_GUEST_PATH_LENGTH: usize = 4096;

pub fn is_guest_path(path: &str) -> bool {
    if path.len() > MAX_GUEST_PATH_LENGTH || !path.starts_with('/') {
        return false;
    }
    if path == "/" {
        return true;
    }
    path[1..].split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|c| !matches!(c, '/' | '\\' | '"' | '\'') && (c as u32) > 0x1f)
    })
}

validated_string_public!(GuestPath, "a guest path", is_guest_path);

impl GuestPath {
    pub fn root() -> Self {
        Self("/".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemEntry {
    pub name: String,
    pub kind: FilesystemEntryKind,
    pub size_bytes: u64,
    pub modified_at: Timestamp,
}

pub const DIRECTORY_ENTRY_LIMIT: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryListing {
    pub path: GuestPath,
    pub entries: Vec<FilesystemEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub measured_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantLogStream {
    Stdout,
    Stderr,
}

impl TenantLogStream {
    pub fn as_str(self) -> &'static str {
        match self {
            TenantLogStream::Stdout => "stdout",
            TenantLogStream::Stderr => "stderr",
        }
    }
}

pub const LOG_SOURCES: [&str; 5] = ["tenant", "agent", "firecracker", "zerofs", "caddy"];

pub const LOG_STREAM_FIELDS: [&str; 3] = ["hostId", "SOURCE", "appId"];

pub const MAX_LOG_CHUNK_LENGTH: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantLogRecord {
    #[serde(rename = "_time")]
    pub time: Timestamp,
    #[serde(rename = "_msg")]
    pub msg: String,
    #[serde(rename = "hostId")]
    pub host_id: HostId,
    #[serde(rename = "SOURCE")]
    pub source: String,
    #[serde(rename = "appId")]
    pub app_id: AppId,
    #[serde(rename = "deploymentId")]
    pub deployment_id: DeploymentId,
    pub stream: TenantLogStream,
    #[serde(rename = "sourceId")]
    pub source_id: String,
    pub sequence: u64,
    #[serde(rename = "droppedBytes", default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
}

pub const DEFAULT_LOG_TIMERANGE: &str = "5m";
