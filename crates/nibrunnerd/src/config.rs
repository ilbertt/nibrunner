use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use protocol::Ipv4Address;

pub const DEFAULT_CONFIG_FILE: &str = "/etc/nibrunner/config.toml";
pub const CONFIG_FILE_VARIABLE: &str = "NIBRUNNER_CONFIG";

const MAX_STORAGE_PREFIX_BYTES: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path} could not be read: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("{path} is not a configuration this host can read: {reason}")]
    Malformed { path: String, reason: String },
    #[error("{field} is not {rule}")]
    Invalid { field: String, rule: String },
}

impl ConfigError {
    pub fn message(&self) -> String {
        self.to_string()
    }

    fn invalid(field: &str, rule: impl std::fmt::Display) -> Self {
        Self::Invalid {
            field: field.to_string(),
            rule: rule.to_string(),
        }
    }
}

/// Where a volume's blocks live. The settings travel with the backend that reads them, so a host
/// cannot name a zerofs mount it will never use, nor pick zerofs and leave it unaddressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeBackend {
    LocalFile,
    Zerofs(ZerofsSettings),
}

impl VolumeBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LocalFile => "local-file",
            Self::Zerofs(_) => "zerofs",
        }
    }

    pub fn zerofs(&self) -> Option<&ZerofsSettings> {
        match self {
            Self::LocalFile => None,
            Self::Zerofs(settings) => Some(settings),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZerofsSettings {
    pub binary: PathBuf,
    pub config_file: PathBuf,
    pub mount_path: PathBuf,
    pub nbd_socket_path: PathBuf,
    pub checkpoint_runtime_dir: PathBuf,
    pub checkpoint_config_file: PathBuf,
    pub checkpoint_cache_dir: PathBuf,
}

/// What this host is reachable on. Each listener is a section that is either absent or complete:
/// there is no half-configured TLS to warn about at startup because there is no way to write one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub http: Option<HttpListener>,
    pub https: Option<HttpsListener>,
    pub port_relay_public_ipv4: Option<Ipv4Address>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpListener {
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpsListener {
    pub port: u16,
    pub certificate: PathBuf,
    pub key: PathBuf,
    /// Naming a trust pool makes a caller's own certificate the price of the handshake.
    pub client_ca: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsConfig {
    pub port: u16,
    pub listen_address: IpAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub guest_image_dir: PathBuf,
    pub firecracker_dir: PathBuf,
    pub desired_state_file: PathBuf,
    pub api_socket: PathBuf,
    pub versions_file: PathBuf,
    pub artifact_store_url: String,
    pub storage_prefix: String,
    pub volumes: VolumeBackend,
    pub control_plane_cidrs_v4: Vec<String>,
    pub control_plane_cidrs_v6: Vec<String>,
    pub proxy: ProxyConfig,
    pub metrics: Option<MetricsConfig>,
    pub export_store_url: String,
    pub export_staging_dir: PathBuf,
}

impl HostConfig {
    pub fn in_state_dir(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }

    pub fn state_db_file(&self) -> PathBuf {
        self.in_state_dir("state.db")
    }

    pub fn instances_file(&self) -> PathBuf {
        self.in_state_dir("instances.json")
    }

    pub fn slots_file(&self) -> PathBuf {
        self.in_state_dir("slots.json")
    }

    pub fn slot_cursor_file(&self) -> PathBuf {
        self.in_state_dir("slot-cursor.json")
    }

    pub fn activity_file(&self) -> PathBuf {
        self.in_state_dir("activity.json")
    }

    pub fn host_id_file(&self) -> PathBuf {
        self.in_state_dir("host-id")
    }

    pub fn cached_desired_state_file(&self) -> PathBuf {
        self.in_state_dir("desired-state.json")
    }

    pub fn deleted_volumes_file(&self) -> PathBuf {
        self.in_state_dir("deleted-volumes.json")
    }

    pub fn artifact_cache_dir(&self) -> PathBuf {
        self.in_state_dir("artifacts")
    }

    pub fn vm_dir(&self) -> PathBuf {
        self.in_state_dir("vm")
    }

    pub fn volumes_dir(&self) -> PathBuf {
        self.in_state_dir("volumes")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.in_state_dir("logs")
    }
}

mod file {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct ConfigFile {
        pub(super) paths: Option<Paths>,
        pub(super) artifacts: Option<Artifacts>,
        pub(super) volumes: Option<Volumes>,
        pub(super) exports: Option<Exports>,
        pub(super) network: Option<Network>,
        pub(super) proxy: Option<Proxy>,
        pub(super) metrics: Option<Metrics>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Paths {
        pub(super) state_dir: Option<String>,
        pub(super) runtime_dir: Option<String>,
        pub(super) snapshot_dir: Option<String>,
        pub(super) guest_image_dir: Option<String>,
        pub(super) desired_state_file: Option<String>,
        pub(super) api_socket: Option<String>,
        pub(super) versions_file: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Artifacts {
        pub(super) store_url: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Exports {
        pub(super) store_url: Option<String>,
        pub(super) staging_dir: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Volumes {
        pub(super) backend: Option<String>,
        pub(super) storage_prefix: Option<String>,
        pub(super) zerofs: Option<Zerofs>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Zerofs {
        pub(super) binary: Option<String>,
        pub(super) config_file: Option<String>,
        pub(super) mount_path: Option<String>,
        pub(super) nbd_socket_path: Option<String>,
        pub(super) checkpoint_runtime_dir: Option<String>,
        pub(super) checkpoint_config_file: Option<String>,
        pub(super) checkpoint_cache_dir: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Network {
        pub(super) control_plane_cidrs_v4: Option<Vec<String>>,
        pub(super) control_plane_cidrs_v6: Option<Vec<String>>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Proxy {
        pub(super) http: Option<Http>,
        pub(super) https: Option<Https>,
        pub(super) port_relay: Option<PortRelay>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Http {
        pub(super) port: Option<u16>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Https {
        pub(super) port: Option<u16>,
        pub(super) certificate: Option<String>,
        pub(super) key: Option<String>,
        pub(super) client_ca: Option<ClientCa>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct ClientCa {
        pub(super) certificate: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct PortRelay {
        pub(super) public_ipv4: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Metrics {
        pub(super) port: Option<u16>,
        pub(super) listen_address: Option<String>,
    }
}

impl HostConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let named = std::env::var(CONFIG_FILE_VARIABLE)
            .ok()
            .filter(|named| !named.is_empty());
        Self::from_file(std::path::Path::new(
            named.as_deref().unwrap_or(DEFAULT_CONFIG_FILE),
        ))
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|error| ConfigError::Unreadable {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        Self::from_toml(&text).map_err(|error| match error {
            ConfigError::Malformed { reason, .. } => ConfigError::Malformed {
                path: path.display().to_string(),
                reason,
            },
            other => other,
        })
    }

    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let document: file::ConfigFile = toml::from_str(text).map_err(|error| ConfigError::Malformed {
            path: "the configuration".to_string(),
            reason: error.message().trim().replace('\n', "; "),
        })?;
        Self::from_document(&document)
    }

    fn from_document(document: &file::ConfigFile) -> Result<Self, ConfigError> {
        let paths = required("paths", document.paths.as_ref())?;
        let state_dir = absolute(
            "paths.state_dir",
            required_str("paths.state_dir", &paths.state_dir)?,
        )?;
        let runtime_dir = absolute(
            "paths.runtime_dir",
            required_str("paths.runtime_dir", &paths.runtime_dir)?,
        )?;

        let volumes = required("volumes", document.volumes.as_ref())?;
        let backend = match required_str("volumes.backend", &volumes.backend)? {
            "local-file" => {
                if volumes.zerofs.is_some() {
                    return Err(ConfigError::invalid(
                        "volumes.zerofs",
                        "read by the local-file backend, which is what volumes.backend says",
                    ));
                }
                VolumeBackend::LocalFile
            }
            "zerofs" => {
                let zerofs = required("volumes.zerofs", volumes.zerofs.as_ref())?;
                VolumeBackend::Zerofs(ZerofsSettings {
                    binary: path_key("volumes.zerofs.binary", &zerofs.binary)?,
                    config_file: path_key("volumes.zerofs.config_file", &zerofs.config_file)?,
                    mount_path: path_key("volumes.zerofs.mount_path", &zerofs.mount_path)?,
                    nbd_socket_path: path_key("volumes.zerofs.nbd_socket_path", &zerofs.nbd_socket_path)?,
                    checkpoint_runtime_dir: path_key(
                        "volumes.zerofs.checkpoint_runtime_dir",
                        &zerofs.checkpoint_runtime_dir,
                    )?,
                    checkpoint_config_file: path_key(
                        "volumes.zerofs.checkpoint_config_file",
                        &zerofs.checkpoint_config_file,
                    )?,
                    checkpoint_cache_dir: path_key(
                        "volumes.zerofs.checkpoint_cache_dir",
                        &zerofs.checkpoint_cache_dir,
                    )?,
                })
            }
            named => {
                return Err(ConfigError::invalid(
                    "volumes.backend",
                    format!("a backend this host has, and there is no {named}"),
                ))
            }
        };

        let network = required("network", document.network.as_ref())?;
        let exports = required("exports", document.exports.as_ref())?;
        let artifacts = required("artifacts", document.artifacts.as_ref())?;

        let proxy = proxy(document.proxy.as_ref())?;
        let metrics = metrics(document.metrics.as_ref(), &proxy)?;

        Ok(Self {
            snapshot_dir: path_key("paths.snapshot_dir", &paths.snapshot_dir)?,
            guest_image_dir: path_key("paths.guest_image_dir", &paths.guest_image_dir)?,
            firecracker_dir: runtime_dir.join("firecracker"),
            desired_state_file: path_key("paths.desired_state_file", &paths.desired_state_file)?,
            api_socket: path_key("paths.api_socket", &paths.api_socket)?,
            versions_file: path_key("paths.versions_file", &paths.versions_file)?,
            artifact_store_url: object_store_url(
                "artifacts.store_url",
                required_str("artifacts.store_url", &artifacts.store_url)?,
            )?,
            storage_prefix: storage_prefix(
                "volumes.storage_prefix",
                required_str("volumes.storage_prefix", &volumes.storage_prefix)?,
            )?,
            volumes: backend,
            control_plane_cidrs_v4: cidrs(
                "network.control_plane_cidrs_v4",
                required(
                    "network.control_plane_cidrs_v4",
                    network.control_plane_cidrs_v4.as_ref(),
                )?,
                Family::V4,
            )?,
            control_plane_cidrs_v6: cidrs(
                "network.control_plane_cidrs_v6",
                required(
                    "network.control_plane_cidrs_v6",
                    network.control_plane_cidrs_v6.as_ref(),
                )?,
                Family::V6,
            )?,
            proxy,
            metrics,
            export_store_url: object_store_url(
                "exports.store_url",
                required_str("exports.store_url", &exports.store_url)?,
            )?,
            export_staging_dir: path_key("exports.staging_dir", &exports.staging_dir)?,
            state_dir,
            runtime_dir,
        })
    }

    pub fn under(root: &std::path::Path) -> Self {
        Self {
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
            snapshot_dir: root.join("state/snapshots"),
            guest_image_dir: root.join("guest"),
            firecracker_dir: root.join("run/firecracker"),
            desired_state_file: root.join("state/desired.json"),
            api_socket: root.join("run/nibrunner.sock"),
            versions_file: root.join("state/versions.json"),
            artifact_store_url: root.join("state/artifact-store").display().to_string(),
            storage_prefix: "volumes".to_string(),
            volumes: VolumeBackend::LocalFile,
            control_plane_cidrs_v4: vec![],
            control_plane_cidrs_v6: vec![],
            proxy: ProxyConfig::default(),
            metrics: None,
            export_store_url: root.join("state/export-store").display().to_string(),
            export_staging_dir: root.join("state/exports"),
        }
    }
}

fn proxy(document: Option<&file::Proxy>) -> Result<ProxyConfig, ConfigError> {
    let Some(document) = document else {
        return Ok(ProxyConfig::default());
    };
    let http = document
        .http
        .as_ref()
        .map(|http| {
            Ok::<_, ConfigError>(HttpListener {
                port: listener("proxy.http.port", required("proxy.http.port", http.port)?)?,
            })
        })
        .transpose()?;
    let https = document
        .https
        .as_ref()
        .map(|https| {
            Ok::<_, ConfigError>(HttpsListener {
                port: listener("proxy.https.port", required("proxy.https.port", https.port)?)?,
                certificate: path_key("proxy.https.certificate", &https.certificate)?,
                key: path_key("proxy.https.key", &https.key)?,
                client_ca: https
                    .client_ca
                    .as_ref()
                    .map(|pool| path_key("proxy.https.client_ca.certificate", &pool.certificate))
                    .transpose()?,
            })
        })
        .transpose()?;
    if let (Some(http), Some(https)) = (&http, &https) {
        if http.port == https.port {
            return Err(ConfigError::invalid(
                "proxy.https.port",
                format!(
                    "a different port from proxy.http.port, which is also {}",
                    http.port
                ),
            ));
        }
    }
    Ok(ProxyConfig {
        http,
        https,
        port_relay_public_ipv4: document
            .port_relay
            .as_ref()
            .map(|relay| {
                let named = required_str("proxy.port_relay.public_ipv4", &relay.public_ipv4)?;
                Ipv4Address::parse(named.trim().to_string())
                    .map_err(|_| ConfigError::invalid("proxy.port_relay.public_ipv4", "an IPv4 address"))
            })
            .transpose()?,
    })
}

fn metrics(
    document: Option<&file::Metrics>,
    proxy: &ProxyConfig,
) -> Result<Option<MetricsConfig>, ConfigError> {
    let Some(document) = document else {
        return Ok(None);
    };
    let port = listener("metrics.port", required("metrics.port", document.port)?)?;
    for (named, field) in [
        (proxy.http.as_ref().map(|http| http.port), "proxy.http.port"),
        (proxy.https.as_ref().map(|https| https.port), "proxy.https.port"),
    ] {
        if named == Some(port) {
            return Err(ConfigError::invalid(
                "metrics.port",
                format!("a different port from {field}, which is also {port}"),
            ));
        }
    }
    // A scrape surface names every app this host runs and what each is using, so where it is bound
    // is said out loud rather than guessed at.
    let listen_address = required_str("metrics.listen_address", &document.listen_address)?
        .trim()
        .parse()
        .map_err(|_| ConfigError::invalid("metrics.listen_address", "an IP address to bind"))?;
    Ok(Some(MetricsConfig { port, listen_address }))
}

/// Nothing this daemon reads has a value it may leave out: a key that is here is a key the
/// configuration states, so absence is never a second meaning to work out at startup.
fn required<T>(field: &str, value: Option<T>) -> Result<T, ConfigError> {
    value.ok_or_else(|| ConfigError::invalid(field, "specified, and nothing here is optional"))
}

fn required_str<'a>(field: &str, value: &'a Option<String>) -> Result<&'a str, ConfigError> {
    required(field, value.as_deref())
}

fn path_key(field: &str, value: &Option<String>) -> Result<PathBuf, ConfigError> {
    absolute(field, required_str(field, value)?)
}

fn absolute(field: &str, value: &str) -> Result<PathBuf, ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::invalid(field, "a path"));
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(ConfigError::invalid(
            field,
            format!("an absolute path, but {trimmed} is not"),
        ));
    }
    Ok(path)
}

fn listener(field: &str, port: u16) -> Result<u16, ConfigError> {
    if port == 0 {
        return Err(ConfigError::invalid(
            field,
            "a port, and 0 is the kernel picking one",
        ));
    }
    let last_slot = u16::try_from(nft_render::SLOT_COUNT.saturating_sub(1)).unwrap_or(u16::MAX);
    for (base, what) in [
        (nft_render::HOST_PORT_BASE, "an app's loopback port"),
        (nft_render::EXTRA_PUBLIC_PORT_BASE, "an app's extra public port"),
    ] {
        let end = base.saturating_add(last_slot);
        if (base..=end).contains(&port) {
            return Err(ConfigError::invalid(
                field,
                format!("free, because {base}-{end} is what a slot takes for {what}"),
            ));
        }
    }
    Ok(port)
}

enum Family {
    V4,
    V6,
}

fn cidrs(field: &str, values: &[String], family: Family) -> Result<Vec<String>, ConfigError> {
    values
        .iter()
        .map(|value| {
            let value = value.trim();
            let (address, length) = value.split_once('/').ok_or_else(|| {
                ConfigError::invalid(field, format!("a CIDR range, but {value} has no prefix length"))
            })?;
            let bits: u8 = length.parse().map_err(|_| {
                ConfigError::invalid(
                    field,
                    format!("a CIDR range, but {length} is not a prefix length"),
                )
            })?;
            let widest = match family {
                Family::V4 => {
                    address.parse::<Ipv4Addr>().map_err(|_| {
                        ConfigError::invalid(
                            field,
                            format!("an IPv4 range, but {address} is not an IPv4 address"),
                        )
                    })?;
                    32
                }
                Family::V6 => {
                    address.parse::<Ipv6Addr>().map_err(|_| {
                        ConfigError::invalid(
                            field,
                            format!("an IPv6 range, but {address} is not an IPv6 address"),
                        )
                    })?;
                    128
                }
            };
            if bits > widest {
                return Err(ConfigError::invalid(
                    field,
                    format!("a range, and /{bits} is wider than the {widest} bits an address has"),
                ));
            }
            Ok(value.to_string())
        })
        .collect()
}

fn object_store_url(field: &str, value: &str) -> Result<String, ConfigError> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("s3://") {
        let bucket = rest.split('/').next().unwrap_or_default();
        if bucket.is_empty() {
            return Err(ConfigError::invalid(field, "an s3:// URL with a bucket in it"));
        }
        return Ok(value.to_string());
    }
    if value.contains("://") {
        let scheme = value.split_once("://").map_or(value, |(scheme, _)| scheme);
        return Err(ConfigError::invalid(
            field,
            format!("a store this host can reach, and there is no {scheme} backend"),
        ));
    }
    Ok(absolute(field, value)?.display().to_string())
}

fn storage_prefix(field: &str, value: &str) -> Result<String, ConfigError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ConfigError::invalid(
            field,
            "a prefix, and an empty one names the bucket root",
        ));
    }
    if value.len() > MAX_STORAGE_PREFIX_BYTES {
        return Err(ConfigError::invalid(
            field,
            format!(
                "at most {MAX_STORAGE_PREFIX_BYTES} bytes, and this is {}",
                value.len()
            ),
        ));
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(ConfigError::invalid(
            field,
            "a prefix without a leading or trailing /",
        ));
    }
    if value
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ConfigError::invalid(
            field,
            "a prefix whose every segment names something",
        ));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The smallest document this daemon accepts. Every key it reads is in here, because there is
    /// no key it will supply for itself, so a test about one key starts from the whole document.
    const WHOLE: &str = r#"[paths]
state_dir = "/var/lib/nibrunner"
runtime_dir = "/run/nibrunner"
snapshot_dir = "/var/lib/nibrunner/snapshots"
guest_image_dir = "/var/lib/nibrunner/guest"
desired_state_file = "/var/lib/nibrunner/desired.json"
api_socket = "/run/nibrunner/nibrunner.sock"
versions_file = "/var/lib/nibrunner/versions.json"

[artifacts]
store_url = "/var/lib/nibrunner/artifact-store"

[volumes]
backend = "local-file"
storage_prefix = "volumes"

[exports]
store_url = "/var/lib/nibrunner/export-store"
staging_dir = "/var/lib/nibrunner/exports"

[network]
control_plane_cidrs_v4 = []
control_plane_cidrs_v6 = []
"#;

    /// `WHOLE` with some keys written differently, and anything in `extra` appended. A field named
    /// here that the document does not have is a typo, not a new key, so it fails loudly.
    fn document(changes: &[(&str, &str)], extra: &str) -> String {
        let mut section = String::new();
        let mut seen: Vec<&str> = vec![];
        let mut out: Vec<String> = vec![];
        for line in WHOLE.lines() {
            if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
                section = name.to_string();
                out.push(line.to_string());
                continue;
            }
            let Some((key, _)) = line.split_once(" = ") else {
                out.push(line.to_string());
                continue;
            };
            let field = format!("{section}.{key}");
            match changes.iter().find(|(named, _)| *named == field) {
                Some((named, value)) => {
                    seen.push(named);
                    out.push(format!("{key} = {value}"));
                }
                None => out.push(line.to_string()),
            }
        }
        for (named, _) in changes {
            assert!(seen.contains(named), "{named} is not a key this document has");
        }
        out.push(extra.to_string());
        out.join("\n")
    }

    fn whole() -> String {
        document(&[], "")
    }

    fn with(extra: &str) -> HostConfig {
        parsed(&document(&[], extra))
    }

    /// `WHOLE` with one key struck out, which is the only way a key can now be absent.
    fn without(field: &str) -> String {
        let mut section = String::new();
        let mut found = false;
        let mut out: Vec<String> = vec![];
        for line in WHOLE.lines() {
            if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
                section = name.to_string();
            } else if let Some((key, _)) = line.split_once(" = ") {
                if format!("{section}.{key}") == field {
                    found = true;
                    continue;
                }
            }
            out.push(line.to_string());
        }
        assert!(found, "{field} is not a key this document has");
        out.join("\n")
    }

    fn parsed(text: &str) -> HostConfig {
        HostConfig::from_toml(text).unwrap()
    }

    fn refused(text: &str) -> String {
        HostConfig::from_toml(text).unwrap_err().message()
    }

    #[test]
    fn a_key_the_document_leaves_out_is_refused_rather_than_filled_in() {
        for field in [
            "paths.state_dir",
            "paths.runtime_dir",
            "paths.snapshot_dir",
            "paths.guest_image_dir",
            "paths.desired_state_file",
            "paths.api_socket",
            "paths.versions_file",
            "artifacts.store_url",
            "volumes.backend",
            "volumes.storage_prefix",
            "exports.store_url",
            "exports.staging_dir",
            "network.control_plane_cidrs_v4",
            "network.control_plane_cidrs_v6",
        ] {
            let message = refused(&without(field));
            assert!(message.contains(field), "{field}: {message}");
            assert!(message.contains("nothing here is optional"), "{field}: {message}");
        }
    }

    #[test]
    fn a_section_this_daemon_reads_is_refused_when_the_document_has_none() {
        for section in ["paths", "artifacts", "volumes", "exports", "network"] {
            let stripped: String = WHOLE
                .split("\n\n")
                .filter(|block| !block.starts_with(&format!("[{section}]")))
                .collect::<Vec<_>>()
                .join("\n\n");
            let message = refused(&stripped);
            assert!(message.contains(section), "{section}: {message}");
        }
    }

    #[test]
    fn a_host_with_no_configuration_file_does_not_start_on_guesses() {
        let error = HostConfig::from_file(Path::new("/nonexistent/nibrunner/config.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Unreadable { .. }), "{error}");
    }

    #[test]
    fn every_file_a_host_writes_is_under_the_directory_the_document_names() {
        let config = parsed(&whole());
        assert_eq!(config.state_dir, PathBuf::from("/var/lib/nibrunner"));
        assert_eq!(
            config.desired_state_file,
            PathBuf::from("/var/lib/nibrunner/desired.json")
        );
        assert_eq!(config.api_socket, PathBuf::from("/run/nibrunner/nibrunner.sock"));
        assert_eq!(
            config.firecracker_dir,
            PathBuf::from("/run/nibrunner/firecracker")
        );
        assert_eq!(config.storage_prefix, "volumes");
        assert_eq!(config.proxy, ProxyConfig::default());
        assert_eq!(config.metrics, None);
    }

    #[test]
    fn a_key_this_daemon_does_not_have_is_refused_by_name() {
        let message = refused(&document(&[], "[proxy.http]\nprot = 80\n"));
        assert!(message.contains("prot"), "{message}");
    }

    #[test]
    fn a_section_this_daemon_does_not_have_is_refused_too() {
        let message = refused(&document(&[], "[zerofs]\nbinary = \"/usr/bin/zerofs\"\n"));
        assert!(message.contains("zerofs"), "{message}");
    }

    #[test]
    fn a_relative_path_is_refused_because_it_names_a_different_place_each_time() {
        let message = refused(&document(&[("paths.state_dir", "\"var/lib/nibrunner\"")], ""));
        assert!(message.contains("paths.state_dir"), "{message}");
        assert!(message.contains("absolute"), "{message}");
    }

    #[test]
    fn a_path_that_is_only_whitespace_names_nothing_and_is_refused() {
        let message = refused(&document(&[("paths.snapshot_dir", "\"   \"")], ""));
        assert!(message.contains("paths.snapshot_dir"), "{message}");
        assert!(message.contains("is not a path"), "{message}");
        assert_eq!(
            with("[proxy.https]\nport = 443\ncertificate = \"  /tls/origin.crt  \"\nkey = \"/tls/origin.key\"\n")
                .proxy
                .https
                .unwrap()
                .certificate,
            PathBuf::from("/tls/origin.crt")
        );
    }

    #[test]
    fn a_proxy_port_a_slot_would_take_is_refused() {
        let message = refused(&document(&[], "[proxy.http]\nport = 21000\n"));
        assert!(message.contains("proxy.http.port"), "{message}");
        assert!(message.contains("21000"), "{message}");
        let extra = refused(&document(
            &[],
            "[proxy.https]\nport = 22062\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n",
        ));
        assert!(extra.contains("22000"), "{extra}");
        assert_eq!(
            with("[proxy.http]\nport = 23000\n").proxy.http.unwrap().port,
            23000
        );
    }

    #[test]
    fn a_port_the_kernel_would_pick_is_not_a_port_this_host_can_be_found_on() {
        let message = refused(&document(&[], "[proxy.http]\nport = 0\n"));
        assert!(message.contains("proxy.http.port"), "{message}");
        assert!(message.contains("kernel picking one"), "{message}");
    }

    #[test]
    fn one_port_cannot_serve_both_plaintext_and_tls() {
        let message = refused(&document(
            &[],
            "[proxy.http]\nport = 8080\n\n[proxy.https]\nport = 8080\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n",
        ));
        assert!(message.contains("proxy.https.port"), "{message}");
    }

    #[test]
    fn a_listener_is_named_whole_or_not_at_all() {
        let message = refused(&document(&[], "[proxy.https]\nport = 8443\n"));
        assert!(message.contains("proxy.https.certificate"), "{message}");
        let half = refused(&document(
            &[],
            "[proxy.https]\nport = 8443\ncertificate = \"/tls/origin.crt\"\n",
        ));
        assert!(half.contains("proxy.https.key"), "{half}");
        let pool = refused(&document(
            &[],
            "[proxy.https]\nport = 8443\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n\n[proxy.https.client_ca]\n",
        ));
        assert!(pool.contains("proxy.https.client_ca.certificate"), "{pool}");
    }

    #[test]
    fn a_trust_pool_is_only_reachable_where_something_serves_tls_to_check_a_caller_against_it() {
        let message = refused(&document(
            &[],
            "[proxy.https.client_ca]\ncertificate = \"/tls/origin-pull-ca.pem\"\n",
        ));
        assert!(message.contains("proxy.https.port"), "{message}");

        let served = with("[proxy.https]\nport = 8443\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n\n[proxy.https.client_ca]\ncertificate = \"/tls/ca.pem\"\n");
        assert_eq!(
            served.proxy.https.as_ref().unwrap().client_ca,
            Some(PathBuf::from("/tls/ca.pem"))
        );
        let open = with("[proxy.https]\nport = 8443\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n");
        assert_eq!(open.proxy.https.unwrap().client_ca, None);
    }

    #[test]
    fn an_address_no_packet_could_be_relayed_to_is_refused() {
        let message = refused(&document(
            &[],
            "[proxy.port_relay]\npublic_ipv4 = \"not.an.address\"\n",
        ));
        assert!(message.contains("proxy.port_relay.public_ipv4"), "{message}");
        assert!(refused(&document(&[], "[proxy.port_relay]\npublic_ipv4 = \"fd00::1\"\n")).contains("IPv4"));
        assert!(refused(&document(&[], "[proxy.port_relay]\n")).contains("nothing here is optional"));
        assert_eq!(
            with("[proxy.port_relay]\npublic_ipv4 = \"203.0.113.10\"\n")
                .proxy
                .port_relay_public_ipv4
                .map(|address| address.to_string()),
            Some("203.0.113.10".to_string())
        );
    }

    #[test]
    fn a_scrape_surface_is_bound_where_the_document_says_and_nowhere_otherwise() {
        assert_eq!(parsed(&whole()).metrics, None);

        let on = with("[metrics]\nport = 9100\nlisten_address = \"0.0.0.0\"\n");
        assert_eq!(
            on.metrics,
            Some(MetricsConfig {
                port: 9100,
                listen_address: IpAddr::from([0, 0, 0, 0]),
            })
        );

        assert!(refused(&document(&[], "[metrics]\nport = 9100\n")).contains("metrics.listen_address"));
        assert!(
            refused(&document(&[], "[metrics]\nlisten_address = \"127.0.0.1\"\n")).contains("metrics.port")
        );
        assert!(refused(&document(
            &[],
            "[metrics]\nport = 21000\nlisten_address = \"127.0.0.1\"\n"
        ))
        .contains("a slot takes"));
        assert!(refused(&document(
            &[],
            "[metrics]\nport = 9100\nlisten_address = \"here\"\n"
        ))
        .contains("metrics.listen_address"));
        let clash = refused(&document(
            &[],
            "[proxy.http]\nport = 9100\n\n[metrics]\nport = 9100\nlisten_address = \"127.0.0.1\"\n",
        ));
        assert!(clash.contains("proxy.http.port"), "{clash}");
    }

    #[test]
    fn a_range_that_nft_would_reject_is_refused_before_the_ruleset_is_rendered() {
        let bad = |value: &str| refused(&document(&[("network.control_plane_cidrs_v4", value)], ""));
        assert!(bad("[\"172.31.0.0\"]").contains("prefix length"));
        assert!(bad("[\"172.31.0.0/33\"]").contains("wider"));
        assert!(bad("[\"fd00::/8\"]").contains("IPv4"));
        assert!(bad("[\"10.0.0.0/eight\"]").contains("not a prefix length"));
        assert!(refused(&document(
            &[("network.control_plane_cidrs_v6", "[\"172.31.0.0/16\"]")],
            ""
        ))
        .contains("IPv6"));
        assert!(refused(&document(
            &[("network.control_plane_cidrs_v6", "[\"fd00::/129\"]")],
            ""
        ))
        .contains("wider"));
        assert_eq!(
            parsed(&document(
                &[("network.control_plane_cidrs_v4", "[\"172.31.0.0/16\"]")],
                ""
            ))
            .control_plane_cidrs_v4,
            vec!["172.31.0.0/16".to_string()]
        );
        assert_eq!(
            parsed(&document(
                &[("network.control_plane_cidrs_v6", "[\" fd00::/8 \"]")],
                ""
            ))
            .control_plane_cidrs_v6,
            vec!["fd00::/8".to_string()]
        );
    }

    #[test]
    fn a_store_this_host_has_no_backend_for_is_refused_at_startup() {
        let store = |value: &str| document(&[("artifacts.store_url", value)], "");
        assert!(refused(&store("\"gs://bucket\"")).contains("no gs backend"));
        assert!(refused(&store("\"s3://\"")).contains("bucket"));
        assert!(refused(&store("\"srv/artifacts\"")).contains("absolute"));
        assert_eq!(
            parsed(&store("\"s3://nibrun/artifacts\"")).artifact_store_url,
            "s3://nibrun/artifacts"
        );
        assert_eq!(
            parsed(&document(&[("exports.store_url", "\"s3://nibrun-exports\"")], "")).export_store_url,
            "s3://nibrun-exports"
        );
    }

    #[test]
    fn a_prefix_that_would_become_a_key_nobody_can_find_is_refused() {
        for bad in ["/volumes", "volumes/", "", "volumes//app", "volumes/../etc"] {
            let text = document(&[("volumes.storage_prefix", &format!("\"{bad}\""))], "");
            assert!(
                HostConfig::from_toml(&text).is_err(),
                "{bad} was accepted as a storage prefix"
            );
        }
        let too_long = "a".repeat(MAX_STORAGE_PREFIX_BYTES + 1);
        assert!(refused(&document(
            &[("volumes.storage_prefix", &format!("\"{too_long}\""))],
            ""
        ))
        .contains("at most 512 bytes"));
        let longest = "b".repeat(MAX_STORAGE_PREFIX_BYTES);
        assert_eq!(
            parsed(&document(
                &[("volumes.storage_prefix", &format!("\"{longest}\""))],
                ""
            ))
            .storage_prefix,
            longest
        );
        assert_eq!(
            parsed(&document(
                &[("volumes.storage_prefix", "\"hosts/one/volumes\"")],
                ""
            ))
            .storage_prefix,
            "hosts/one/volumes"
        );
    }

    #[test]
    fn a_backend_travels_with_the_settings_it_reads_and_no_others() {
        assert_eq!(parsed(&whole()).volumes, VolumeBackend::LocalFile);
        assert_eq!(VolumeBackend::LocalFile.as_str(), "local-file");
        assert!(refused(&document(&[("volumes.backend", "\"nfs\"")], "")).contains("no nfs"),);

        let orphaned = refused(&document(&[], ZEROFS));
        assert!(orphaned.contains("volumes.zerofs"), "{orphaned}");
        assert!(orphaned.contains("local-file"), "{orphaned}");

        let unaddressed = refused(&document(&[("volumes.backend", "\"zerofs\"")], ""));
        assert!(unaddressed.contains("volumes.zerofs"), "{unaddressed}");
    }

    const ZEROFS: &str = r#"[volumes.zerofs]
binary = "/usr/local/bin/zerofs"
config_file = "/etc/zerofs/one.toml"
mount_path = "/srv/zerofs"
nbd_socket_path = "/run/zerofs/one.sock"
checkpoint_runtime_dir = "/run/zerofs-checkpoints"
checkpoint_config_file = "/etc/zerofs/checkpoint.toml"
checkpoint_cache_dir = "/data/zerofs-checkpoint"
"#;

    #[test]
    fn where_this_hosts_zerofs_is_can_be_moved_whole() {
        let config = parsed(&document(&[("volumes.backend", "\"zerofs\"")], ZEROFS));
        assert_eq!(config.volumes.as_str(), "zerofs");
        let zerofs = config.volumes.zerofs().unwrap();
        assert_eq!(zerofs.binary, PathBuf::from("/usr/local/bin/zerofs"));
        assert_eq!(
            zerofs.checkpoint_runtime_dir,
            PathBuf::from("/run/zerofs-checkpoints")
        );
        assert!(refused(&document(
            &[("volumes.backend", "\"zerofs\"")],
            &ZEROFS.replace("/srv/zerofs", "srv/zerofs")
        ))
        .contains("volumes.zerofs.mount_path"));
        assert!(refused(&document(
            &[("volumes.backend", "\"zerofs\"")],
            "[volumes.zerofs]\nbinary = \"/usr/local/bin/zerofs\"\n"
        ))
        .contains("volumes.zerofs.config_file"));
    }

    #[test]
    fn a_finished_bundle_is_never_kept_inside_the_tree_the_reap_removes() {
        for staging in ["\"/var/lib/nibrunner/exports\"", "\"/mnt/scratch/exports\""] {
            let config = parsed(&document(&[("exports.staging_dir", staging)], ""));
            let store = PathBuf::from(&config.export_store_url);
            assert!(
                !store.starts_with(&config.export_staging_dir),
                "{} is inside {}",
                store.display(),
                config.export_staging_dir.display()
            );
        }
    }

    #[test]
    fn a_malformed_document_names_the_file_it_came_from() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[proxy\n").unwrap();
        let message = HostConfig::from_file(&path).unwrap_err().message();
        assert!(message.contains("config.toml"), "{message}");
    }

    #[test]
    fn the_sample_this_repository_ships_is_a_configuration_this_daemon_accepts() {
        let sample = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/config.toml");
        let text = std::fs::read_to_string(sample).unwrap();
        assert_eq!(HostConfig::from_toml(&text).unwrap(), parsed(&whole()));
    }

    #[test]
    fn a_host_rooted_under_one_directory_keeps_every_file_it_writes_inside_it() {
        let root = Path::new("/srv/one-host");
        let config = HostConfig::under(root);
        for path in [
            config.state_db_file(),
            config.instances_file(),
            config.slots_file(),
            config.slot_cursor_file(),
            config.activity_file(),
            config.host_id_file(),
            config.cached_desired_state_file(),
            config.deleted_volumes_file(),
            config.artifact_cache_dir(),
            config.vm_dir(),
            config.volumes_dir(),
            config.logs_dir(),
            config.desired_state_file.clone(),
            config.api_socket.clone(),
            config.versions_file.clone(),
            config.export_staging_dir.clone(),
            config.firecracker_dir.clone(),
        ] {
            assert!(
                path.starts_with(root),
                "{} escapes {}",
                path.display(),
                root.display()
            );
        }
        assert_eq!(config.in_state_dir("anything"), root.join("state/anything"));
        assert_eq!(config.volumes, VolumeBackend::LocalFile);
        assert_eq!(config.proxy, ProxyConfig::default());
    }

    #[test]
    fn every_file_this_host_keeps_has_a_name_of_its_own() {
        let config = HostConfig::under(Path::new("/srv/one-host"));
        let named = [
            config.state_db_file(),
            config.instances_file(),
            config.slots_file(),
            config.slot_cursor_file(),
            config.activity_file(),
            config.host_id_file(),
            config.cached_desired_state_file(),
            config.deleted_volumes_file(),
            config.artifact_cache_dir(),
            config.vm_dir(),
            config.volumes_dir(),
            config.logs_dir(),
        ];
        let distinct: std::collections::BTreeSet<_> = named.iter().collect();
        assert_eq!(distinct.len(), named.len());
    }

    #[test]
    fn a_whole_document_reads_back_as_it_was_written() {
        let config = parsed(&document(
            &[
                ("paths.state_dir", "\"/srv/nibrunner\""),
                ("paths.snapshot_dir", "\"/mnt/cache/snapshots\""),
                ("artifacts.store_url", "\"s3://nibrun-artifacts/prod\""),
                ("network.control_plane_cidrs_v4", "[\"172.31.0.0/16\"]"),
            ],
            r#"[proxy.http]
port = 80

[proxy.https]
port = 443
certificate = "/etc/nibrunner/origin.crt"
key = "/etc/nibrunner/origin.key"

[proxy.https.client_ca]
certificate = "/etc/nibrunner/origin-pull-ca.pem"

[proxy.port_relay]
public_ipv4 = "203.0.113.10"

[metrics]
port = 9100
listen_address = "127.0.0.1"
"#,
        ));
        assert_eq!(config.state_dir, PathBuf::from("/srv/nibrunner"));
        assert_eq!(config.snapshot_dir, PathBuf::from("/mnt/cache/snapshots"));
        assert_eq!(config.artifact_store_url, "s3://nibrun-artifacts/prod");
        assert_eq!(config.control_plane_cidrs_v4, vec!["172.31.0.0/16".to_string()]);
        assert_eq!(config.proxy.http, Some(HttpListener { port: 80 }));
        assert_eq!(
            config.proxy.https,
            Some(HttpsListener {
                port: 443,
                certificate: PathBuf::from("/etc/nibrunner/origin.crt"),
                key: PathBuf::from("/etc/nibrunner/origin.key"),
                client_ca: Some(PathBuf::from("/etc/nibrunner/origin-pull-ca.pem")),
            })
        );
        assert_eq!(
            config
                .proxy
                .port_relay_public_ipv4
                .map(|address| address.to_string()),
            Some("203.0.113.10".to_string())
        );
        assert_eq!(
            config.metrics,
            Some(MetricsConfig {
                port: 9100,
                listen_address: IpAddr::from([127, 0, 0, 1]),
            })
        );
    }
}
