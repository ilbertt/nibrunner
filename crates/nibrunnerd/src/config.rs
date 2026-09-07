use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use protocol::Ipv4Address;

const DEFAULT_STATE_DIR: &str = "/var/lib/nibrunner";
const DEFAULT_RUNTIME_DIR: &str = "/run/nibrunner";
const DEFAULT_SNAPSHOT_DIR: &str = "/var/lib/nibrunner/snapshots";
const DEFAULT_GUEST_IMAGE_DIR: &str = "/var/lib/nibrunner/guest";
const DEFAULT_STORAGE_PREFIX: &str = "volumes";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeBackendKind {
    LocalFile,
    Zerofs,
}

impl VolumeBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalFile => "local-file",
            Self::Zerofs => "zerofs",
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

const DEFAULT_ZEROFS_BINARY: &str = "/opt/nibrun/bin/zerofs/zerofs";
const DEFAULT_ZEROFS_CONFIG: &str = "/etc/zerofs/config.toml";
const DEFAULT_ZEROFS_MOUNT: &str = "/mnt/zerofs";
const DEFAULT_ZEROFS_NBD_SOCKET: &str = "/run/zerofs/nbd.sock";
const DEFAULT_ZEROFS_CHECKPOINT_RUNTIME_DIR: &str = "/run/zerofs-checkpoint";
const DEFAULT_ZEROFS_CHECKPOINT_CONFIG: &str = "/etc/zerofs/checkpoint.toml";
const DEFAULT_ZEROFS_CHECKPOINT_CACHE_DIR: &str = "/data/zerofs-checkpoint";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub guest_image_dir: PathBuf,
    pub firecracker_dir: PathBuf,
    pub desired_state_file: PathBuf,
    pub api_socket: PathBuf,
    pub artifact_store_url: String,
    pub volume_store_url: Option<String>,
    pub storage_prefix: String,
    pub volume_backend: VolumeBackendKind,
    pub zerofs: Option<ZerofsSettings>,
    pub port_relay_public_ipv4: Option<Ipv4Address>,
    pub control_plane_cidrs_v4: Vec<String>,
    pub control_plane_cidrs_v6: Vec<String>,
    pub proxy_https_port: Option<u16>,
    pub proxy_http_port: Option<u16>,
    pub proxy_tls_certificate: Option<PathBuf>,
    pub proxy_tls_key: Option<PathBuf>,
    pub control_plane_url: Option<String>,
    pub versions_file: PathBuf,
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

    pub fn tls_material(&self) -> Option<(&Path, &Path)> {
        match (&self.proxy_tls_certificate, &self.proxy_tls_key) {
            (Some(certificate), Some(key)) => Some((certificate.as_path(), key.as_path())),
            _ => None,
        }
    }
}

mod file {
    use serde::Deserialize;

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct ConfigFile {
        #[serde(default)]
        pub(super) paths: Paths,
        #[serde(default)]
        pub(super) artifacts: Artifacts,
        #[serde(default)]
        pub(super) volumes: Volumes,
        #[serde(default)]
        pub(super) proxy: Proxy,
        #[serde(default)]
        pub(super) network: Network,
        #[serde(default)]
        pub(super) control_plane: ControlPlane,
        #[serde(default)]
        pub(super) exports: Exports,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Exports {
        pub(super) store_url: Option<String>,
        pub(super) staging_dir: Option<String>,
    }

    #[derive(Debug, Default, Deserialize)]
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

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Artifacts {
        pub(super) store_url: Option<String>,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Volumes {
        pub(super) backend: Option<String>,
        pub(super) store_url: Option<String>,
        pub(super) storage_prefix: Option<String>,
        pub(super) zerofs: Option<Zerofs>,
    }

    #[derive(Debug, Default, Deserialize)]
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

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Proxy {
        pub(super) http_port: Option<u16>,
        pub(super) https_port: Option<u16>,
        pub(super) tls_certificate: Option<String>,
        pub(super) tls_key: Option<String>,
        pub(super) port_relay_public_ipv4: Option<String>,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Network {
        #[serde(default)]
        pub(super) control_plane_cidrs_v4: Vec<String>,
        #[serde(default)]
        pub(super) control_plane_cidrs_v6: Vec<String>,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct ControlPlane {
        pub(super) url: Option<String>,
    }
}

impl HostConfig {
    pub fn load() -> Result<Self, ConfigError> {
        match std::env::var(CONFIG_FILE_VARIABLE)
            .ok()
            .filter(|named| !named.is_empty())
        {
            Some(named) => Self::from_file(Path::new(&named)),
            None => {
                let default = Path::new(DEFAULT_CONFIG_FILE);
                if default.exists() {
                    Self::from_file(default)
                } else {
                    Self::from_document(&file::ConfigFile::default())
                }
            }
        }
    }

    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
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
        let paths = &document.paths;
        let state_dir = directory("paths.state_dir", paths.state_dir.as_deref(), DEFAULT_STATE_DIR)?;
        let runtime_dir = directory(
            "paths.runtime_dir",
            paths.runtime_dir.as_deref(),
            DEFAULT_RUNTIME_DIR,
        )?;

        let http = document
            .proxy
            .http_port
            .map(|port| listener("proxy.http_port", port))
            .transpose()?;
        let https = document
            .proxy
            .https_port
            .map(|port| listener("proxy.https_port", port))
            .transpose()?;
        if let (Some(http), Some(https)) = (http, https) {
            if http == https {
                return Err(ConfigError::invalid(
                    "proxy.https_port",
                    format!("a different port from proxy.http_port, which is also {http}"),
                ));
            }
        }

        let backend = match document.volumes.backend.as_deref() {
            None | Some("local-file") => VolumeBackendKind::LocalFile,
            Some("zerofs") => VolumeBackendKind::Zerofs,
            Some(named) => {
                return Err(ConfigError::invalid(
                    "volumes.backend",
                    format!("a backend this host has, and there is no {named}"),
                ))
            }
        };
        if document.volumes.zerofs.is_some() && backend != VolumeBackendKind::Zerofs {
            return Err(ConfigError::invalid(
                "volumes.zerofs",
                format!(
                    "read by any backend but {}, which is what volumes.backend says",
                    backend.as_str()
                ),
            ));
        }
        let zerofs_settings = match backend {
            VolumeBackendKind::LocalFile => None,
            VolumeBackendKind::Zerofs => {
                let named = document.volumes.zerofs.as_ref();
                Some(ZerofsSettings {
                    binary: directory(
                        "volumes.zerofs.binary",
                        named.and_then(|zerofs| zerofs.binary.as_deref()),
                        DEFAULT_ZEROFS_BINARY,
                    )?,
                    config_file: directory(
                        "volumes.zerofs.config_file",
                        named.and_then(|zerofs| zerofs.config_file.as_deref()),
                        DEFAULT_ZEROFS_CONFIG,
                    )?,
                    mount_path: directory(
                        "volumes.zerofs.mount_path",
                        named.and_then(|zerofs| zerofs.mount_path.as_deref()),
                        DEFAULT_ZEROFS_MOUNT,
                    )?,
                    nbd_socket_path: directory(
                        "volumes.zerofs.nbd_socket_path",
                        named.and_then(|zerofs| zerofs.nbd_socket_path.as_deref()),
                        DEFAULT_ZEROFS_NBD_SOCKET,
                    )?,
                    checkpoint_runtime_dir: directory(
                        "volumes.zerofs.checkpoint_runtime_dir",
                        named.and_then(|zerofs| zerofs.checkpoint_runtime_dir.as_deref()),
                        DEFAULT_ZEROFS_CHECKPOINT_RUNTIME_DIR,
                    )?,
                    checkpoint_config_file: directory(
                        "volumes.zerofs.checkpoint_config_file",
                        named.and_then(|zerofs| zerofs.checkpoint_config_file.as_deref()),
                        DEFAULT_ZEROFS_CHECKPOINT_CONFIG,
                    )?,
                    checkpoint_cache_dir: directory(
                        "volumes.zerofs.checkpoint_cache_dir",
                        named.and_then(|zerofs| zerofs.checkpoint_cache_dir.as_deref()),
                        DEFAULT_ZEROFS_CHECKPOINT_CACHE_DIR,
                    )?,
                })
            }
        };

        Ok(Self {
            snapshot_dir: directory(
                "paths.snapshot_dir",
                paths.snapshot_dir.as_deref(),
                DEFAULT_SNAPSHOT_DIR,
            )?,
            guest_image_dir: directory(
                "paths.guest_image_dir",
                paths.guest_image_dir.as_deref(),
                DEFAULT_GUEST_IMAGE_DIR,
            )?,
            firecracker_dir: runtime_dir.join("firecracker"),
            desired_state_file: beneath(
                "paths.desired_state_file",
                paths.desired_state_file.as_deref(),
                &state_dir,
                "desired.json",
            )?,
            api_socket: beneath(
                "paths.api_socket",
                paths.api_socket.as_deref(),
                &runtime_dir,
                "nibrunner.sock",
            )?,
            versions_file: beneath(
                "paths.versions_file",
                paths.versions_file.as_deref(),
                &state_dir,
                "versions.json",
            )?,
            artifact_store_url: match document.artifacts.store_url.as_deref() {
                Some(url) => object_store_url("artifacts.store_url", url)?,
                None => state_dir.join("artifact-store").display().to_string(),
            },
            volume_store_url: document
                .volumes
                .store_url
                .as_deref()
                .map(|url| object_store_url("volumes.store_url", url))
                .transpose()?,
            storage_prefix: match document.volumes.storage_prefix.as_deref() {
                Some(prefix) => storage_prefix("volumes.storage_prefix", prefix)?,
                None => DEFAULT_STORAGE_PREFIX.to_string(),
            },
            volume_backend: backend,
            zerofs: zerofs_settings,
            port_relay_public_ipv4: document
                .proxy
                .port_relay_public_ipv4
                .as_deref()
                .map(|value| {
                    Ipv4Address::parse(value.to_string())
                        .map_err(|_| ConfigError::invalid("proxy.port_relay_public_ipv4", "an IPv4 address"))
                })
                .transpose()?,
            control_plane_cidrs_v4: cidrs(
                "network.control_plane_cidrs_v4",
                &document.network.control_plane_cidrs_v4,
                Family::V4,
            )?,
            control_plane_cidrs_v6: cidrs(
                "network.control_plane_cidrs_v6",
                &document.network.control_plane_cidrs_v6,
                Family::V6,
            )?,
            proxy_http_port: http,
            proxy_https_port: https,
            proxy_tls_certificate: document
                .proxy
                .tls_certificate
                .as_deref()
                .map(|path| absolute("proxy.tls_certificate", path))
                .transpose()?,
            proxy_tls_key: document
                .proxy
                .tls_key
                .as_deref()
                .map(|path| absolute("proxy.tls_key", path))
                .transpose()?,
            control_plane_url: document
                .control_plane
                .url
                .as_deref()
                .map(|url| control_plane_url("control_plane.url", url))
                .transpose()?,
            export_store_url: match document.exports.store_url.as_deref() {
                Some(url) => object_store_url("exports.store_url", url)?,
                None => state_dir.join("export-store").display().to_string(),
            },
            export_staging_dir: beneath(
                "exports.staging_dir",
                document.exports.staging_dir.as_deref(),
                &state_dir,
                "exports",
            )?,
            state_dir,
            runtime_dir,
        })
    }

    pub fn under(root: &Path) -> Self {
        Self {
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
            snapshot_dir: root.join("state/snapshots"),
            guest_image_dir: root.join("guest"),
            firecracker_dir: root.join("run/firecracker"),
            desired_state_file: root.join("state/desired.json"),
            api_socket: root.join("run/nibrunner.sock"),
            artifact_store_url: root.join("state/artifact-store").display().to_string(),
            volume_store_url: None,
            storage_prefix: DEFAULT_STORAGE_PREFIX.to_string(),
            volume_backend: VolumeBackendKind::LocalFile,
            zerofs: None,
            port_relay_public_ipv4: None,
            control_plane_cidrs_v4: vec![],
            control_plane_cidrs_v6: vec![],
            proxy_https_port: None,
            proxy_http_port: None,
            proxy_tls_certificate: None,
            proxy_tls_key: None,
            control_plane_url: None,
            versions_file: root.join("state/versions.json"),
            export_store_url: root.join("state/export-store").display().to_string(),
            export_staging_dir: root.join("state/exports"),
        }
    }
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

fn directory(field: &str, value: Option<&str>, fallback: &str) -> Result<PathBuf, ConfigError> {
    match value {
        Some(value) => absolute(field, value),
        None => Ok(PathBuf::from(fallback)),
    }
}

fn beneath(field: &str, value: Option<&str>, parent: &Path, name: &str) -> Result<PathBuf, ConfigError> {
    match value {
        Some(value) => absolute(field, value),
        None => Ok(parent.join(name)),
    }
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

fn control_plane_url(field: &str, value: &str) -> Result<String, ConfigError> {
    let value = value.trim().trim_end_matches('/');
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .ok_or_else(|| ConfigError::invalid(field, "an http:// or https:// URL"))?;
    if rest.is_empty() {
        return Err(ConfigError::invalid(field, "a URL with a host in it"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> HostConfig {
        HostConfig::from_toml(text).unwrap()
    }

    fn refused(text: &str) -> String {
        HostConfig::from_toml(text).unwrap_err().message()
    }

    #[test]
    fn a_host_that_names_nothing_still_has_somewhere_to_put_everything() {
        let config = parsed("");
        assert_eq!(config.state_dir, PathBuf::from(DEFAULT_STATE_DIR));
        assert_eq!(
            config.desired_state_file,
            PathBuf::from("/var/lib/nibrunner/desired.json")
        );
        assert_eq!(config.api_socket, PathBuf::from("/run/nibrunner/nibrunner.sock"));
        assert_eq!(config.storage_prefix, "volumes");
        assert_eq!(config.tls_material(), None);
    }

    #[test]
    fn what_was_not_named_follows_the_directory_it_belongs_to() {
        let config = parsed("[paths]\nstate_dir = \"/srv/nibrunner\"\nruntime_dir = \"/run/nbr\"\n");
        assert_eq!(
            config.desired_state_file,
            PathBuf::from("/srv/nibrunner/desired.json")
        );
        assert_eq!(
            config.instances_file(),
            PathBuf::from("/srv/nibrunner/instances.json")
        );
        assert_eq!(config.api_socket, PathBuf::from("/run/nbr/nibrunner.sock"));
        assert_eq!(config.firecracker_dir, PathBuf::from("/run/nbr/firecracker"));
    }

    #[test]
    fn a_key_this_daemon_does_not_have_is_refused_by_name() {
        let message = refused("[proxy]\nhttp_prot = 80\n");
        assert!(message.contains("http_prot"), "{message}");
    }

    #[test]
    fn a_section_this_daemon_does_not_have_is_refused_too() {
        let message = refused("[zerofs]\nbinary = \"/usr/bin/zerofs\"\n");
        assert!(message.contains("zerofs"), "{message}");
    }

    #[test]
    fn a_relative_path_is_refused_because_it_names_a_different_place_each_time() {
        let message = refused("[paths]\nstate_dir = \"var/lib/nibrunner\"\n");
        assert!(message.contains("paths.state_dir"), "{message}");
        assert!(message.contains("absolute"), "{message}");
    }

    #[test]
    fn a_proxy_port_a_slot_would_take_is_refused() {
        let message = refused("[proxy]\nhttp_port = 21000\n");
        assert!(message.contains("proxy.http_port"), "{message}");
        assert!(message.contains("21000"), "{message}");
        let extra = refused("[proxy]\nhttps_port = 22062\n");
        assert!(extra.contains("22000"), "{extra}");
        assert_eq!(
            parsed("[proxy]\nhttp_port = 21063\n").proxy_http_port,
            Some(21063)
        );
    }

    #[test]
    fn one_port_cannot_serve_both_plaintext_and_tls() {
        let message = refused("[proxy]\nhttp_port = 8080\nhttps_port = 8080\n");
        assert!(message.contains("proxy.https_port"), "{message}");
    }

    #[test]
    fn a_range_that_nft_would_reject_is_refused_before_the_ruleset_is_rendered() {
        assert!(refused("[network]\ncontrol_plane_cidrs_v4 = [\"172.31.0.0\"]\n").contains("prefix length"));
        assert!(refused("[network]\ncontrol_plane_cidrs_v4 = [\"172.31.0.0/33\"]\n").contains("wider"));
        assert!(refused("[network]\ncontrol_plane_cidrs_v4 = [\"fd00::/8\"]\n").contains("IPv4"));
        assert!(refused("[network]\ncontrol_plane_cidrs_v6 = [\"172.31.0.0/16\"]\n").contains("IPv6"));
        assert_eq!(
            parsed("[network]\ncontrol_plane_cidrs_v4 = [\"172.31.0.0/16\"]\n").control_plane_cidrs_v4,
            vec!["172.31.0.0/16".to_string()]
        );
    }

    #[test]
    fn a_store_this_host_has_no_backend_for_is_refused_at_startup() {
        assert!(refused("[artifacts]\nstore_url = \"gs://bucket\"\n").contains("no gs backend"));
        assert!(refused("[artifacts]\nstore_url = \"s3://\"\n").contains("bucket"));
        assert_eq!(
            parsed("[artifacts]\nstore_url = \"s3://nibrun/artifacts\"\n").artifact_store_url,
            "s3://nibrun/artifacts"
        );
        assert_eq!(
            parsed("[artifacts]\nstore_url = \"/srv/artifacts\"\n").artifact_store_url,
            "/srv/artifacts"
        );
    }

    #[test]
    fn a_prefix_that_would_become_a_key_nobody_can_find_is_refused() {
        for bad in ["/volumes", "volumes/", "", "volumes//app", "volumes/../etc"] {
            let text = format!("[volumes]\nstorage_prefix = \"{bad}\"\n");
            assert!(
                HostConfig::from_toml(&text).is_err(),
                "{bad} was accepted as a storage prefix"
            );
        }
        assert_eq!(
            parsed("[volumes]\nstorage_prefix = \"hosts/one/volumes\"\n").storage_prefix,
            "hosts/one/volumes"
        );
    }

    #[test]
    fn a_host_keeps_its_volumes_on_its_own_disk_unless_it_says_otherwise() {
        assert_eq!(parsed("").volume_backend, VolumeBackendKind::LocalFile);
        assert_eq!(parsed("").zerofs, None);
        let zerofs = parsed("[volumes]\nbackend = \"zerofs\"\n");
        assert_eq!(zerofs.volume_backend, VolumeBackendKind::Zerofs);
        assert_eq!(
            zerofs.zerofs.as_ref().map(|settings| settings.mount_path.clone()),
            Some(PathBuf::from("/mnt/zerofs"))
        );
        assert!(refused("[volumes]\nbackend = \"nfs\"\n").contains("no nfs"));
    }

    #[test]
    fn zerofs_settings_under_a_backend_that_would_not_read_them_are_refused() {
        let message = refused("[volumes.zerofs]\nmount_path = \"/mnt/zerofs\"\n");
        assert!(message.contains("volumes.zerofs"), "{message}");
        assert!(message.contains("local-file"), "{message}");
    }

    #[test]
    fn where_this_hosts_zerofs_is_can_be_moved_whole() {
        let zerofs = parsed(
            r#"
[volumes]
backend = "zerofs"

[volumes.zerofs]
binary = "/usr/local/bin/zerofs"
config_file = "/etc/zerofs/one.toml"
mount_path = "/srv/zerofs"
nbd_socket_path = "/run/zerofs/one.sock"
checkpoint_runtime_dir = "/run/zerofs-checkpoints"
"#,
        )
        .zerofs
        .unwrap();
        assert_eq!(zerofs.binary, PathBuf::from("/usr/local/bin/zerofs"));
        assert_eq!(
            zerofs.checkpoint_runtime_dir,
            PathBuf::from("/run/zerofs-checkpoints")
        );
        assert!(
            refused("[volumes]\nbackend = \"zerofs\"\n\n[volumes.zerofs]\nmount_path = \"mnt\"\n")
                .contains("volumes.zerofs.mount_path")
        );
    }

    #[test]
    fn a_finished_bundle_is_never_kept_inside_the_tree_the_reap_removes() {
        for text in [
            "",
            "[paths]\nstate_dir = \"/srv/nibrunner\"\n",
            "[exports]\nstaging_dir = \"/mnt/scratch/exports\"\n",
        ] {
            let config = parsed(text);
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
    fn a_control_plane_that_is_not_reachable_over_http_is_refused() {
        assert!(refused("[control_plane]\nurl = \"nibrun.example.com\"\n").contains("http"));
        assert_eq!(
            parsed("[control_plane]\nurl = \"https://nibrun.example.com/\"\n").control_plane_url,
            Some("https://nibrun.example.com".to_string())
        );
    }

    #[test]
    fn a_certificate_without_its_key_is_not_tls_material() {
        let config = parsed("[proxy]\ntls_certificate = \"/tls/origin.crt\"\n");
        assert_eq!(config.tls_material(), None);
        let both = parsed("[proxy]\ntls_certificate = \"/tls/origin.crt\"\ntls_key = \"/tls/origin.key\"\n");
        assert!(both.tls_material().is_some());
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
    fn a_file_that_was_named_and_is_not_there_is_an_error() {
        let error = HostConfig::from_file(Path::new("/nonexistent/nibrunner/config.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Unreadable { .. }), "{error}");
    }

    #[test]
    fn the_sample_this_repository_ships_is_a_configuration_this_daemon_accepts() {
        let sample = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/config.toml");
        let text = std::fs::read_to_string(sample).unwrap();
        assert_eq!(HostConfig::from_toml(&text).unwrap(), parsed(""));
    }

    #[test]
    fn a_port_the_kernel_would_pick_is_not_a_port_this_host_can_be_found_on() {
        let message = refused("[proxy]\nhttp_port = 0\n");
        assert!(message.contains("proxy.http_port"), "{message}");
        assert!(message.contains("kernel picking one"), "{message}");
        assert_eq!(parsed("[proxy]\nhttp_port = 443\n").proxy_http_port, Some(443));
    }

    #[test]
    fn a_path_that_is_only_whitespace_names_nothing_and_is_refused() {
        let message = refused("[proxy]\ntls_certificate = \"   \"\n");
        assert!(message.contains("proxy.tls_certificate"), "{message}");
        assert!(message.contains("is not a path"), "{message}");
        assert_eq!(
            parsed("[proxy]\ntls_key = \"  /tls/origin.key  \"\n").proxy_tls_key,
            Some(PathBuf::from("/tls/origin.key"))
        );
    }

    #[test]
    fn a_url_with_a_scheme_and_no_host_reaches_no_control_plane() {
        let message = refused("[control_plane]\nurl = \"https://\"\n");
        assert!(message.contains("control_plane.url"), "{message}");
        assert!(refused("[control_plane]\nurl = \"ftp://nibrun.example.com\"\n").contains("http"));
        assert_eq!(
            parsed("[control_plane]\nurl = \"http://127.0.0.1:8080//\"\n").control_plane_url,
            Some("http://127.0.0.1:8080".to_string())
        );
    }

    #[test]
    fn a_store_named_by_a_relative_path_would_move_with_the_working_directory() {
        let message = refused("[volumes]\nstore_url = \"srv/volumes\"\n");
        assert!(message.contains("volumes.store_url"), "{message}");
        assert!(message.contains("absolute"), "{message}");
        assert_eq!(
            parsed("[exports]\nstore_url = \"s3://nibrun-exports\"\n").export_store_url,
            "s3://nibrun-exports"
        );
        assert_eq!(parsed("").volume_store_url, None);
    }

    #[test]
    fn a_prefix_longer_than_a_key_may_be_is_refused_by_its_length() {
        let too_long = "a".repeat(MAX_STORAGE_PREFIX_BYTES + 1);
        let message = refused(&format!("[volumes]\nstorage_prefix = \"{too_long}\"\n"));
        assert!(message.contains("at most 512 bytes"), "{message}");
        let longest = "b".repeat(MAX_STORAGE_PREFIX_BYTES);
        assert_eq!(
            parsed(&format!("[volumes]\nstorage_prefix = \"{longest}\"\n")).storage_prefix,
            longest
        );
    }

    #[test]
    fn an_address_no_packet_could_be_relayed_to_is_refused() {
        let message = refused("[proxy]\nport_relay_public_ipv4 = \"not.an.address\"\n");
        assert!(message.contains("proxy.port_relay_public_ipv4"), "{message}");
        assert!(refused("[proxy]\nport_relay_public_ipv4 = \"fd00::1\"\n").contains("IPv4"));
        assert_eq!(
            parsed("[proxy]\nport_relay_public_ipv4 = \"203.0.113.10\"\n")
                .port_relay_public_ipv4
                .map(|address| address.to_string()),
            Some("203.0.113.10".to_string())
        );
    }

    #[test]
    fn a_key_without_its_certificate_is_no_more_tls_material_than_the_other_way_round() {
        assert_eq!(
            parsed("[proxy]\ntls_key = \"/tls/origin.key\"\n").tls_material(),
            None
        );
    }

    #[test]
    fn a_range_that_is_written_with_room_around_it_is_still_the_range_it_names() {
        assert_eq!(
            parsed("[network]\ncontrol_plane_cidrs_v6 = [\" fd00::/8 \"]\n").control_plane_cidrs_v6,
            vec!["fd00::/8".to_string()]
        );
        assert!(refused("[network]\ncontrol_plane_cidrs_v6 = [\"fd00::/129\"]\n").contains("wider"));
        assert!(
            refused("[network]\ncontrol_plane_cidrs_v4 = [\"10.0.0.0/eight\"]\n")
                .contains("not a prefix length")
        );
        assert!(parsed("").control_plane_cidrs_v4.is_empty());
    }

    #[test]
    fn every_backend_this_host_has_is_named_the_way_the_configuration_spells_it() {
        assert_eq!(VolumeBackendKind::LocalFile.as_str(), "local-file");
        assert_eq!(VolumeBackendKind::Zerofs.as_str(), "zerofs");
        assert_eq!(
            parsed("[volumes]\nbackend = \"local-file\"\n").volume_backend,
            VolumeBackendKind::LocalFile
        );
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
        assert_eq!(config.volume_backend, VolumeBackendKind::LocalFile);
        assert_eq!(config.tls_material(), None);
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
        let config = parsed(
            r#"
[paths]
state_dir = "/srv/nibrunner"
runtime_dir = "/run/nibrunner"
snapshot_dir = "/mnt/cache/snapshots"
guest_image_dir = "/srv/guest"

[artifacts]
store_url = "s3://nibrun-artifacts/prod"

[volumes]
store_url = "s3://nibrun-volumes"
storage_prefix = "volumes"

[proxy]
http_port = 80
https_port = 443
tls_certificate = "/etc/nibrunner/origin.crt"
tls_key = "/etc/nibrunner/origin.key"
port_relay_public_ipv4 = "203.0.113.10"

[network]
control_plane_cidrs_v4 = ["172.31.0.0/16"]

[control_plane]
url = "https://nibrun.example.com"
"#,
        );
        assert_eq!(config.snapshot_dir, PathBuf::from("/mnt/cache/snapshots"));
        assert_eq!(config.artifact_store_url, "s3://nibrun-artifacts/prod");
        assert_eq!(config.volume_store_url, Some("s3://nibrun-volumes".to_string()));
        assert_eq!(config.proxy_http_port, Some(80));
        assert_eq!(config.proxy_https_port, Some(443));
        assert!(config.tls_material().is_some());
        assert_eq!(
            config.port_relay_public_ipv4.map(|address| address.to_string()),
            Some("203.0.113.10".to_string())
        );
    }
}
