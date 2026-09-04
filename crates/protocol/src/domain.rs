//! The domain records both ends read: what an app, an instance, a volume and a host are.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wire::*;

// ---------------------------------------------------------------------------------------------
// app.ts

const ENVIRONMENT_RESERVED_NAME: &str = "__proto__";

/// `^(?!__proto__$)[A-Za-z_][A-Za-z0-9_]*$`. One name is carved out of what is otherwise the
/// shell's own rule, because a JavaScript object is how an environment travels from the control
/// plane to a host: `environment.__proto__ = value` sets a prototype rather than a property.
pub fn is_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    name != ENVIRONMENT_RESERVED_NAME
        && matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

const RUNTIME_VALUE_PREFIX: &str = "NIBRUN_";

/// Every runtime value the guest sets, spelled as it is written. What every other end reads
/// rather than restates: `reference_value` in `apps/runtime/src/config.c` is the one place that
/// cannot import it.
pub const RUNTIME_VALUE_NAMES: [&str; 5] = [
    "NIBRUN_DATA_DIR",
    "NIBRUN_EXTRA_PUBLIC_PORT",
    "NIBRUN_HOSTNAME",
    "NIBRUN_HTTP_PORT",
    "NIBRUN_PUBLIC_IPV4",
];

/// The two the guest is only given when the app asked for a public port besides HTTP.
pub const EXTRA_PUBLIC_PORT_VALUES: [&str; 2] = ["NIBRUN_EXTRA_PUBLIC_PORT", "NIBRUN_PUBLIC_IPV4"];

fn is_name_character(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The references a value holds: each is `(name, allowed)` where `allowed` says the guest offers
/// it. A `$` that opens no `NIBRUN_` reference is left alone, which is what leaves a bcrypt hash
/// and a literal `$HOME` alone.
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

/// Whether every runtime value `value` names is one the guest offers, which most values name
/// none of.
pub fn names_offered_runtime_values(value: &str) -> bool {
    runtime_references(value).iter().all(|(_, allowed)| *allowed)
}

/// Whether `value` names a runtime value only an app with an extra public port is given.
pub fn names_extra_public_port_values(value: &str) -> bool {
    runtime_references(value)
        .iter()
        .any(|(name, allowed)| *allowed && EXTRA_PUBLIC_PORT_VALUES.contains(&name.as_str()))
}

/// A runtime value as it is named in a tenant value, which is the form worth showing back.
pub fn interpolable_runtime_value(name: &str) -> String {
    format!("${{{name}}}")
}

/// A tenant value as the guest reads it. Refused here rather than in the guest, while whoever
/// typed it is still listening.
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

/// Closed, because a name the pattern does not match is otherwise neither validated nor
/// rejected. Ordered, so what is rendered from it is rendered the same way twice.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "BTreeMap<String, TenantValue>", into = "BTreeMap<String, TenantValue>")]
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

/// Mirrored by CONFIG_MAX_ARGUMENTS in apps/runtime, which refuses a file exceeding it.
pub const MAX_ARGUMENTS: usize = 64;
pub const MAX_ARGUMENT_LENGTH: usize = 4096;

/// argv[1..] for the tenant binary; argv[0] is always the binary itself. A list rather than one
/// string: splitting a command line means quoting rules, and the value the user typed reaching
/// exec unchanged is worth more than the convenience.
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

/// `platform` is the subdomain nibrun issues; `custom` is a domain the user brought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppHostnameKind {
    Platform,
    Custom,
}

/// No state: a host is sent the hostnames it should be answering for, and one it should not
/// answer for yet is left out rather than sent with a flag saying so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppHostname {
    pub hostname: Hostname,
    pub kind: AppHostnameKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub http_port: HttpPort,
    /// Whether the app is reached on a public TCP and UDP port besides HTTP. A yes or no rather
    /// than a number, because the port has to be the same on every hop for a binary that
    /// announces the one it bound to be announcing a reachable one.
    pub has_extra_public_port: bool,
    pub args: TenantArguments,
    pub environment: TenantEnvironment,
    pub resources: InstanceResources,
    pub health_check: HealthCheck,
    pub restart_policy: RestartPolicy,
}

/// Whether the app's microVM is kept up or is brought up by a request for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppActivation {
    Always,
    OnRequest,
}

/// The floor is the cadence the sleep decision is made on, so a shorter timeout would be one the
/// host accepts and cannot keep.
pub const MIN_IDLE_TIMEOUT_MS: u64 = 60_000;
/// A day: generous enough that it can only ever refuse a slipped zero.
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

// ---------------------------------------------------------------------------------------------
// instance.ts

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

/// 256 leaves a Bun server several times its resident baseline while doubling density against
/// the 512 it replaces.
pub const DEFAULT_INSTANCE_RESOURCES: InstanceResources = InstanceResources {
    vcpu_count: 1,
    memory_mib: 256,
};

/// Run by the host against the HTTP port the user declared. A bare TCP connect is the default
/// because that is precisely the question being asked; a path upgrades the probe to an HTTP GET
/// that must answer 2xx.
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

/// Applied by the guest's init to the tenant process, not by the host to the microVM. When the
/// budget is exhausted the guest powers itself off and the host reports the instance `failed`
/// rather than booting it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_factor: f64,
    /// A process that stayed up this long is treated as healthy and its restart count resets.
    pub reset_after_ms: u64,
}

pub const DEFAULT_RESTART_POLICY: RestartPolicy = RestartPolicy {
    max_restarts: 5,
    initial_backoff_ms: 500,
    max_backoff_ms: 30_000,
    backoff_factor: 2.0,
    reset_after_ms: 60_000,
};

/// `starting` is a booted microVM whose tenant process has not yet accepted a connection, and
/// `running` is one that has. `idle` is an on-request app with no microVM because nothing has
/// asked for one: not `stopped`, because an idle instance is serving and the next request is
/// what it is waiting for.
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

// ---------------------------------------------------------------------------------------------
// volume.ts, checkpoint.ts, export.ts, host.ts

pub const DEFAULT_VOLUME_SIZE_BYTES: u64 = 8_589_934_592;

/// `deleted` is reported once the filesystem is actually gone, which is what lets the control
/// plane finish deleting an app rather than leave it saying `deleting` forever.
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

/// Answering "what is this host running" from the host itself, so it can be compared against
/// what git says it should be running.
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

// ---------------------------------------------------------------------------------------------
// compute.ts, filesystem.ts

/// What a running app is spending on the machine it was given, as the guest kernel accounts for
/// it. `cpuShare` is a rate and everything else here is a level, which is why it can be missing
/// while the rest is not: a rate needs two readings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeUsage {
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_share: Option<f64>,
    pub measured_at: Timestamp,
}

/// `other` is every symlink, socket, fifo and device node: a browser's only question is whether
/// descending is meaningful, and for all of these the answer is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilesystemEntryKind {
    File,
    Directory,
    Other,
}

pub const MAX_ENTRY_NAME_LENGTH: usize = 255;

/// Deliberately permissive: a name is reported, a path is accepted. The tenant's own binary
/// created these, so anything ext4 allows has to survive being described.
pub fn is_entry_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_ENTRY_NAME_LENGTH && !name.contains(['/', '\0'])
}

pub const MAX_GUEST_PATH_LENGTH: usize = 4096;

/// An absolute path inside one tenant's filesystem. `.` and `..` are excluded outright rather
/// than resolved, and quotes and backslashes are excluded because the value once ended up in a
/// command string.
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

/// `truncated` rather than a cursor, while the answer is a single read of a single directory.
pub const DIRECTORY_ENTRY_LIMIT: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryListing {
    pub path: GuestPath,
    pub entries: Vec<FilesystemEntry>,
    pub truncated: bool,
}

/// How full a volume is, as the kernel that has it mounted accounts for it. `usedBytes` is what
/// `df` calls used, so it counts the journal and the metadata ext4 wrote before a tenant existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub measured_at: Timestamp,
}

// ---------------------------------------------------------------------------------------------
// log.ts

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

/// Which component wrote a record. Uppercase, alone among the fields, because it is the only one
/// two very different writers have to agree on: journald requires field names to be uppercase.
pub const LOG_SOURCES: [&str; 5] = ["tenant", "agent", "firecracker", "zerofs", "caddy"];

/// The fields that identify a record's stream, and the only ones that may.
pub const LOG_STREAM_FIELDS: [&str; 3] = ["hostId", "SOURCE", "appId"];

pub const MAX_LOG_CHUNK_LENGTH: usize = 65_536;

/// `_msg` and `_time` are the store's own names for a record's message and timestamp. Everything
/// else stays camelCase, like the rest of the protocol.
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
    /// Recreated with the host receiver. A gap in `sequence` within one `sourceId` means bounded
    /// buffering dropped records; a new `sourceId` means the receiver itself restarted.
    #[serde(rename = "sourceId")]
    pub source_id: String,
    pub sequence: u64,
    #[serde(rename = "droppedBytes", default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
}

pub const DEFAULT_LOG_TIMERANGE: &str = "5m";
