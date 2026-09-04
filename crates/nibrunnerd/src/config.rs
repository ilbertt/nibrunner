//! Everything the daemon reads at startup, from one file, validated once so that nothing further
//! in has to ask whether a setting makes sense.
//!
//! A file rather than the environment. What a host is configured to be is something an operator
//! reads back six months later, and an environment cannot be read back: it is spread across a
//! unit file, a shell profile and whatever started the process, and no two hosts can be diffed.
//! It is also the only shape in which a wrong setting can be *refused* — an environment variable
//! nobody set and one whose name was mistyped are the same absence, so a typo silently takes the
//! default, where an unknown key here is an error that names the line it is on.
//!
//! Validated at startup rather than at first use, for the same reason the ruleset is rendered
//! whole: a host that is going to refuse a setting should refuse it while an operator is still
//! watching, not on the pass that first needed it.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use protocol::Ipv4Address;

const DEFAULT_STATE_DIR: &str = "/var/lib/nibrunner";
const DEFAULT_RUNTIME_DIR: &str = "/run/nibrunner";
/// On a disk that may be a cache, because a snapshot is one: losing one costs a cold boot and
/// nothing else.
const DEFAULT_SNAPSHOT_DIR: &str = "/var/lib/nibrunner/snapshots";
const DEFAULT_GUEST_IMAGE_DIR: &str = "/var/lib/nibrunner/guest";
const DEFAULT_STORAGE_PREFIX: &str = "volumes";

/// Where the file is unless something says otherwise, and the one thing still read from the
/// environment: a process has to be told where to read before it can read anything.
pub const DEFAULT_CONFIG_FILE: &str = "/etc/nibrunner/config.toml";
pub const CONFIG_FILE_VARIABLE: &str = "NIBRUNNER_CONFIG";

/// The longest prefix an object store will be asked to carry. Not a limit of any store; a limit
/// on how wrong a value can be before it is worth saying so, since a prefix is prepended to every
/// key this host ever writes.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub guest_image_dir: PathBuf,
    /// Where the embedded Firecracker is extracted to, under a directory named for its version so
    /// two daemons of different versions on one host cannot overwrite each other's copy.
    pub firecracker_dir: PathBuf,
    /// The document the daemon watches. One of the three sources of the same state.
    pub desired_state_file: PathBuf,
    /// Where anything local reaches this daemon.
    pub api_socket: PathBuf,
    /// Where the artifact bytes come from: a local directory for dev, an S3 URL for a fleet.
    pub artifact_store_url: String,
    /// Where a volume's blocks live, for the backend that keeps them in an object store.
    pub volume_store_url: Option<String>,
    pub storage_prefix: String,
    /// Where a tenant's own public port is reached, which is not this host's address and not
    /// something a guest can discover.
    pub port_relay_public_ipv4: Option<Ipv4Address>,
    pub control_plane_cidrs_v4: Vec<String>,
    pub control_plane_cidrs_v6: Vec<String>,
    /// The proxy's own listener, and the certificate it serves. ACME is deferred, so a host with
    /// no certificate serves HTTP only and says so.
    pub proxy_https_port: Option<u16>,
    pub proxy_http_port: Option<u16>,
    pub proxy_tls_certificate: Option<PathBuf>,
    pub proxy_tls_key: Option<PathBuf>,
    /// The remote control plane, when there is one. Absent is the ordinary case: the file is the
    /// source that ships.
    pub control_plane_url: Option<String>,
    pub versions_file: PathBuf,
}

impl HostConfig {
    pub fn in_state_dir(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }

    pub fn instances_file(&self) -> PathBuf {
        self.in_state_dir("instances.json")
    }

    pub fn slots_file(&self) -> PathBuf {
        self.in_state_dir("slots.json")
    }

    /// Beside the slots rather than inside them: `slots.json` is a plain app-to-slot record that a
    /// daemon either side of this reads the same way, and a key that is not an app in there would
    /// read back as one holding a slot.
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

    /// A daemon with no certificate has no HTTPS listener: serving one from a certificate that is
    /// not there would be a proxy answering handshakes it cannot complete.
    pub fn tls_material(&self) -> Option<(&Path, &Path)> {
        match (&self.proxy_tls_certificate, &self.proxy_tls_key) {
            (Some(certificate), Some(key)) => Some((certificate.as_path(), key.as_path())),
            _ => None,
        }
    }
}

/// The document as written.
///
/// Every field is optional, so an empty file is a valid host and a host that names nothing gets
/// the single-machine defaults. `deny_unknown_fields` on all of it is the point of the exercise:
/// a setting this daemon does not have is a mistake worth stopping for, and it is the one class
/// of mistake an environment could never report.
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
        pub(super) store_url: Option<String>,
        pub(super) storage_prefix: Option<String>,
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
    /// The file the environment names, or the one at the default path, or the defaults.
    ///
    /// A path given explicitly must exist: naming a file is saying it matters, and reading past
    /// it would leave a host running on defaults its operator believes it is not running on. The
    /// default path is allowed to be absent, which is what makes a fresh single-machine host work
    /// with nothing installed but the binary.
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
            // The parse failure carries the line and column; what it cannot carry is which file,
            // because it never saw a path.
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
        // One socket cannot carry both, and a host that asked for it would come up serving
        // whichever bound first: the same daemon answering plaintext on the port it was told to
        // serve TLS on is worse than not starting.
        if let (Some(http), Some(https)) = (http, https) {
            if http == https {
                return Err(ConfigError::invalid(
                    "proxy.https_port",
                    format!("a different port from proxy.http_port, which is also {http}"),
                ));
            }
        }

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
            state_dir,
            runtime_dir,
        })
    }

    /// Everything under one directory, for a test and for `just run-dev`.
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
            port_relay_public_ipv4: None,
            control_plane_cidrs_v4: vec![],
            control_plane_cidrs_v6: vec![],
            proxy_https_port: None,
            proxy_http_port: None,
            proxy_tls_certificate: None,
            proxy_tls_key: None,
            control_plane_url: None,
            versions_file: root.join("state/versions.json"),
        }
    }
}

/// Absolute, because the daemon's working directory is whatever started it: a relative path in a
/// file that outlives the shell that wrote it names a different place every time.
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

/// A path, or a name under the directory it belongs to. The fallback is derived rather than
/// written down twice, so moving `state_dir` moves everything that had not been named separately.
fn beneath(field: &str, value: Option<&str>, parent: &Path, name: &str) -> Result<PathBuf, ConfigError> {
    match value {
        Some(value) => absolute(field, value),
        None => Ok(parent.join(name)),
    }
}

/// A port an app slot already takes cannot also be the proxy's. Slots are allocated without asking
/// anything about the proxy, so the collision surfaces as a listener that will not bind — on a
/// pass that has nothing to do with the proxy, long after whoever wrote the file has gone.
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

/// Validated because these are interpolated into the ruleset as text. `nft` would reject a
/// malformed one, but it rejects the *whole* file: one bad line in this config would leave a host
/// with no isolation at all rather than with one range missing.
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

/// `s3://bucket[/prefix]`, or an absolute directory on this host. Refused here rather than at the
/// first deploy, because a scheme this daemon has no backend for is a host that will fetch
/// nothing, and finding that out at startup costs an operator a minute instead of an outage.
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

/// A key prefix in an object store, not a path on this host. A leading slash makes an empty first
/// segment and `..` means nothing to a bucket, so both are refused here rather than becoming a key
/// nobody can find again.
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

/// Reached over HTTP, and with a host in it. Anything else is a value that would fail on the first
/// poll rather than here.
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

    /// Moving the state directory has to move what was derived from it, or a host is configured
    /// in one place and keeps its notes in another.
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

    /// The whole reason this is a file: a setting that does not exist can be said so, where an
    /// environment variable nobody set and one mistyped are the same absence.
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

    /// The collision would otherwise surface as a listener that will not bind, on a pass that has
    /// nothing to do with the proxy.
    #[test]
    fn a_proxy_port_a_slot_would_take_is_refused() {
        let message = refused("[proxy]\nhttp_port = 21000\n");
        assert!(message.contains("proxy.http_port"), "{message}");
        assert!(message.contains("21000"), "{message}");
        let extra = refused("[proxy]\nhttps_port = 22062\n");
        assert!(extra.contains("22000"), "{extra}");
        // One past the last slot is nobody's.
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

    /// Naming a file is saying it matters, so reading past a missing one would leave a host on
    /// defaults its operator believes it is not on.
    #[test]
    fn a_file_that_was_named_and_is_not_there_is_an_error() {
        let error = HostConfig::from_file(Path::new("/nonexistent/nibrunner/config.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Unreadable { .. }), "{error}");
    }

    /// The annotated copy is what an operator starts from, so it is held to the same rules as
    /// anything they write themselves — and it claims to show every key at its default, which is
    /// only true if what it parses to is the default.
    #[test]
    fn the_sample_this_repository_ships_is_a_configuration_this_daemon_accepts() {
        let sample = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/config.toml");
        let text = std::fs::read_to_string(sample).unwrap();
        assert_eq!(HostConfig::from_toml(&text).unwrap(), parsed(""));
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
