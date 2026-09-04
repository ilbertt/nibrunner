//! Everything the daemon reads from its environment, resolved once at startup so nothing further
//! in has to know an environment variable exists.

use std::path::{Path, PathBuf};

use protocol::Ipv4Address;

const DEFAULT_STATE_DIR: &str = "/var/lib/nibrunner";
const DEFAULT_RUNTIME_DIR: &str = "/run/nibrunner";
/// On a disk that may be a cache, because a snapshot is one: losing one costs a cold boot and
/// nothing else.
const DEFAULT_SNAPSHOT_DIR: &str = "/var/lib/nibrunner/snapshots";
const DEFAULT_GUEST_IMAGE_DIR: &str = "/var/lib/nibrunner/guest";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name} is required")]
    Missing { name: &'static str },
    #[error("{name} is not {rule}")]
    Invalid { name: &'static str, rule: &'static str },
}

impl ConfigError {
    pub fn message(&self) -> String {
        self.to_string()
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
    /// Where `nibrunnerctl` and anything else local reaches this daemon.
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
    /// The remote control plane, when there is one. Absent is the ordinary case in v1: the file
    /// and the socket are the two sources that ship.
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

fn variable(name: &'static str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn path_or(name: &'static str, fallback: &str) -> PathBuf {
    PathBuf::from(variable(name).unwrap_or_else(|| fallback.to_string()))
}

fn list(name: &'static str) -> Vec<String> {
    variable(name)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn port(name: &'static str) -> Result<Option<u16>, ConfigError> {
    match variable(name) {
        None => Ok(None),
        Some(value) => value.parse().map(Some).map_err(|_| ConfigError::Invalid {
            name,
            rule: "a port number",
        }),
    }
}

impl HostConfig {
    /// Read from the environment. A host that names nothing gets a working single-machine
    /// default, which is what makes `run-dev` one command.
    pub fn from_environment() -> Result<Self, ConfigError> {
        let state_dir = path_or("NIBRUNNER_STATE_DIR", DEFAULT_STATE_DIR);
        let runtime_dir = path_or("NIBRUNNER_RUNTIME_DIR", DEFAULT_RUNTIME_DIR);
        let relay = match variable("NIBRUNNER_PORT_RELAY_PUBLIC_IPV4") {
            None => None,
            Some(value) => Some(Ipv4Address::parse(value).map_err(|_| ConfigError::Invalid {
                name: "NIBRUNNER_PORT_RELAY_PUBLIC_IPV4",
                rule: "an IPv4 address",
            })?),
        };
        Ok(Self {
            snapshot_dir: PathBuf::from(
                variable("NIBRUNNER_SNAPSHOT_DIR").unwrap_or_else(|| DEFAULT_SNAPSHOT_DIR.to_string()),
            ),
            guest_image_dir: path_or("NIBRUNNER_GUEST_IMAGE_DIR", DEFAULT_GUEST_IMAGE_DIR),
            firecracker_dir: runtime_dir.join("firecracker"),
            desired_state_file: PathBuf::from(
                variable("NIBRUNNER_DESIRED_STATE_FILE")
                    .unwrap_or_else(|| state_dir.join("desired.json").display().to_string()),
            ),
            api_socket: PathBuf::from(
                variable("NIBRUNNER_API_SOCKET")
                    .unwrap_or_else(|| runtime_dir.join("nibrunner.sock").display().to_string()),
            ),
            versions_file: PathBuf::from(
                variable("NIBRUNNER_VERSIONS_FILE")
                    .unwrap_or_else(|| state_dir.join("versions.json").display().to_string()),
            ),
            artifact_store_url: variable("NIBRUNNER_ARTIFACT_STORE_URL")
                .unwrap_or_else(|| state_dir.join("artifact-store").display().to_string()),
            volume_store_url: variable("NIBRUNNER_VOLUME_STORE_URL"),
            storage_prefix: variable("NIBRUNNER_STORAGE_PREFIX").unwrap_or_else(|| "volumes".to_string()),
            port_relay_public_ipv4: relay,
            control_plane_cidrs_v4: list("NIBRUNNER_CONTROL_PLANE_CIDRS_V4"),
            control_plane_cidrs_v6: list("NIBRUNNER_CONTROL_PLANE_CIDRS_V6"),
            proxy_https_port: port("NIBRUNNER_PROXY_HTTPS_PORT")?,
            proxy_http_port: port("NIBRUNNER_PROXY_HTTP_PORT")?,
            proxy_tls_certificate: variable("NIBRUNNER_TLS_CERTIFICATE").map(PathBuf::from),
            proxy_tls_key: variable("NIBRUNNER_TLS_KEY").map(PathBuf::from),
            control_plane_url: variable("NIBRUNNER_CONTROL_PLANE_URL"),
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
            storage_prefix: "volumes".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_that_names_nothing_still_has_somewhere_to_put_everything() {
        let config = HostConfig::under(Path::new("/tmp/nibrunner-test"));
        assert!(config.instances_file().starts_with(&config.state_dir));
        assert!(config.slot_cursor_file().ends_with("slot-cursor.json"));
        assert_eq!(config.tls_material(), None);
    }

    #[test]
    fn a_certificate_without_its_key_is_not_tls_material() {
        let mut config = HostConfig::under(Path::new("/tmp/nibrunner-test"));
        config.proxy_tls_certificate = Some(PathBuf::from("/tls/origin.crt"));
        assert_eq!(config.tls_material(), None);
        config.proxy_tls_key = Some(PathBuf::from("/tls/origin.key"));
        assert!(config.tls_material().is_some());
    }
}
