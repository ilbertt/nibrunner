use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

pub const DEFAULT_CONFIG_FILE: &str = "/etc/nibrunner/config.toml";
pub const CONFIG_FILE_VARIABLE: &str = "NIBRUNNER_CONFIG";

const MAX_STORAGE_PREFIX_BYTES: usize = 512;
const MEBIBYTES_PER_GIBIBYTE: u64 = 1024;

/// Where ZeroFS serves its own scrape page, on loopback. It is a constant rather than a key
/// because it is nibrun's, and `nibrunnerd install` renders it into ZeroFS's config — but a host
/// that also serves `[metrics]` has to be kept off it, which is why this is here and not there.
pub const ZEROFS_PROMETHEUS_PORT: u16 = 9091;

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
    Zerofs(Box<ZerofsSettings>),
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
            Self::Zerofs(settings) => Some(settings.as_ref()),
        }
    }
}

/// Everything `nibrunnerd install` needs to lay ZeroFS down, and everything the daemon needs to
/// talk to it once systemd has it running. The two config files this describes are rendered from
/// here rather than written beside here, so the cache size the daemon reserves against and the
/// cache size ZeroFS takes are one answer instead of two that drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZerofsSettings {
    pub binary: PathBuf,
    pub config_file: PathBuf,
    pub mount_path: PathBuf,
    pub nbd_socket_path: PathBuf,
    pub ninep_socket_path: PathBuf,
    pub rpc_socket_path: PathBuf,
    pub storage_url: String,
    pub cache_dir: PathBuf,
    /// Held as mebibytes because that is what the rest of this daemon reserves and reports in,
    /// but stated in the file as whole gibibytes — which is all `cache_gigabytes` can read back
    /// out of the rendered config, so a fraction here would reserve against a number ZeroFS is
    /// not taking.
    pub cache_disk_mib: u64,
    pub cache_memory_mib: u64,
    pub checkpoint_runtime_dir: PathBuf,
    pub checkpoint_config_file: PathBuf,
    pub checkpoint_cache_dir: PathBuf,
}

/// Where the world reaches an app on this host.
///
/// Every way in is a section under here, and each is absent or complete: there is no
/// half-configured listener to warn about at startup because there is no way to write one. Each
/// binds an address of its own, because each faces a different machine — HTTP arrives from the
/// edge that terminates TLS for it, a raw port from the relay that publishes it — and a host that
/// put both on one address would be saying they arrive from the same place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub http: Option<HttpListener>,
    pub raw: Option<RawPorts>,
}

/// The one HTTP listener.
///
/// One per host and one per guest: a guest's hostname resolves to this host, and this is what
/// carries it to the one port that guest answers HTTP on. One rather than a plain port beside a
/// TLS port, because nothing here redirects — two would serve every app unencrypted and encrypted
/// at once, forever, with nothing moving a visitor from the first to the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpListener {
    pub listen_address: IpAddr,
    pub port: u16,
    /// Absent serves plain HTTP, which is what a host behind an edge that terminates TLS wants.
    pub tls: Option<TlsMaterial>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsMaterial {
    pub certificate: PathBuf,
    pub key: PathBuf,
    /// Naming a trust pool makes a caller's own certificate the price of the handshake.
    pub client_ca: Option<PathBuf>,
}

/// The ports a guest may answer on beside its HTTP one, carried to it unread.
///
/// A raw port carries a protocol this host does not speak — ssh, DNS, WireGuard — so nothing can
/// route it by name and it is reached at a port of its own. Which protocol carries each is the
/// document's to say, port by port. The HTTP listener is not one of these and is never counted.
///
/// The address is the one a relay reaches this host on. A raw port published to the world is an
/// address a tenant hands to its own users, and that is a machine of its own; this host binds
/// where that machine can see it and nowhere else. Absent is a host that carries nothing raw, and
/// a document asking one of those for such a port is refused rather than quietly left unreachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPorts {
    pub listen_address: IpAddr,
    /// Bounded by what a slot reserves past the port its HTTP listener takes.
    pub max_ports_per_guest: usize,
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
    pub denied_egress_addresses_v4: Vec<String>,
    pub denied_egress_addresses_v6: Vec<String>,
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
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize)]
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

    #[derive(Debug, Serialize, Deserialize)]
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

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Artifacts {
        pub(super) store_url: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Exports {
        pub(super) store_url: Option<String>,
        pub(super) staging_dir: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Volumes {
        pub(super) backend: Option<String>,
        pub(super) storage_prefix: Option<String>,
        pub(super) zerofs: Option<Zerofs>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Zerofs {
        pub(super) binary: Option<String>,
        pub(super) config_file: Option<String>,
        pub(super) mount_path: Option<String>,
        pub(super) nbd_socket_path: Option<String>,
        pub(super) ninep_socket_path: Option<String>,
        pub(super) rpc_socket_path: Option<String>,
        pub(super) storage_url: Option<String>,
        pub(super) cache_dir: Option<String>,
        pub(super) cache_disk_gib: Option<u64>,
        pub(super) cache_memory_gib: Option<u64>,
        pub(super) checkpoint_runtime_dir: Option<String>,
        pub(super) checkpoint_config_file: Option<String>,
        pub(super) checkpoint_cache_dir: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Network {
        pub(super) denied_egress_addresses_v4: Option<Vec<String>>,
        pub(super) denied_egress_addresses_v6: Option<Vec<String>>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Proxy {
        pub(super) http: Option<Http>,
        pub(super) raw: Option<Raw>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Http {
        pub(super) listen_address: Option<String>,
        pub(super) port: Option<u16>,
        pub(super) tls: Option<Tls>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Tls {
        pub(super) certificate: Option<String>,
        pub(super) key: Option<String>,
        pub(super) client_ca: Option<ClientCa>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Raw {
        pub(super) listen_address: Option<String>,
        pub(super) max_ports_per_guest: Option<usize>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct ClientCa {
        pub(super) certificate: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Metrics {
        pub(super) port: Option<u16>,
        pub(super) listen_address: Option<String>,
    }
}

impl HostConfig {
    pub fn load() -> Result<Self, ConfigError> {
        Self::from_file(&Self::configured_file())
    }

    /// The file `load` reads. Named separately because what `install` writes has to point back at
    /// the file the values came from, and pointing at the wrong one is worse than pointing at none.
    pub fn configured_file() -> PathBuf {
        std::env::var(CONFIG_FILE_VARIABLE)
            .ok()
            .filter(|named| !named.trim().is_empty())
            .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE), PathBuf::from)
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
                VolumeBackend::Zerofs(Box::new(ZerofsSettings {
                    binary: path_key("volumes.zerofs.binary", &zerofs.binary)?,
                    config_file: path_key("volumes.zerofs.config_file", &zerofs.config_file)?,
                    mount_path: path_key("volumes.zerofs.mount_path", &zerofs.mount_path)?,
                    nbd_socket_path: path_key("volumes.zerofs.nbd_socket_path", &zerofs.nbd_socket_path)?,
                    ninep_socket_path: path_key(
                        "volumes.zerofs.ninep_socket_path",
                        &zerofs.ninep_socket_path,
                    )?,
                    rpc_socket_path: path_key("volumes.zerofs.rpc_socket_path", &zerofs.rpc_socket_path)?,
                    storage_url: object_store_url(
                        "volumes.zerofs.storage_url",
                        required_str("volumes.zerofs.storage_url", &zerofs.storage_url)?,
                    )?,
                    cache_dir: path_key("volumes.zerofs.cache_dir", &zerofs.cache_dir)?,
                    cache_disk_mib: mebibytes(
                        "volumes.zerofs.cache_disk_gib",
                        required("volumes.zerofs.cache_disk_gib", zerofs.cache_disk_gib)?,
                    )?,
                    cache_memory_mib: mebibytes(
                        "volumes.zerofs.cache_memory_gib",
                        required("volumes.zerofs.cache_memory_gib", zerofs.cache_memory_gib)?,
                    )?,
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
                }))
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
        let metrics = metrics(document.metrics.as_ref(), &proxy, &backend)?;

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
            denied_egress_addresses_v4: cidrs(
                "network.denied_egress_addresses_v4",
                required(
                    "network.denied_egress_addresses_v4",
                    network.denied_egress_addresses_v4.as_ref(),
                )?,
                Family::V4,
            )?,
            denied_egress_addresses_v6: cidrs(
                "network.denied_egress_addresses_v6",
                required(
                    "network.denied_egress_addresses_v6",
                    network.denied_egress_addresses_v6.as_ref(),
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

    /// The starting point, laid out where a Linux distribution would put it: volumes as files on
    /// this machine's own disk, stores as directories on it, plain HTTP on :80 — because a host with
    /// no listener refuses a document that names a hostname, and a host that serves nothing is not
    /// a starting point. It is what `install` writes a host that has none — from here, so there is
    /// no file in the repository to fall behind [`Self::from_document`].
    pub fn starter() -> Self {
        let mut starter = Self::laid_out(
            PathBuf::from("/var/lib/nibrunner"),
            PathBuf::from("/run/nibrunner"),
            PathBuf::from("/var/lib/nibrunner/guest"),
        );
        starter.proxy.http = Some(HttpListener {
            listen_address: IpAddr::from([0, 0, 0, 0]),
            port: 80,
            tls: None,
        });
        starter
    }

    pub fn under(root: &std::path::Path) -> Self {
        Self::laid_out(root.join("state"), root.join("run"), root.join("guest"))
    }

    fn laid_out(state_dir: PathBuf, runtime_dir: PathBuf, guest_image_dir: PathBuf) -> Self {
        Self {
            snapshot_dir: state_dir.join("snapshots"),
            guest_image_dir,
            firecracker_dir: runtime_dir.join("firecracker"),
            desired_state_file: state_dir.join("desired.json"),
            api_socket: runtime_dir.join("nibrunner.sock"),
            versions_file: state_dir.join("versions.json"),
            artifact_store_url: state_dir.join("artifact-store").display().to_string(),
            storage_prefix: "volumes".to_string(),
            volumes: VolumeBackend::LocalFile,
            denied_egress_addresses_v4: vec![],
            denied_egress_addresses_v6: vec![],
            proxy: ProxyConfig::default(),
            metrics: None,
            export_store_url: state_dir.join("export-store").display().to_string(),
            export_staging_dir: state_dir.join("exports"),
            state_dir,
            runtime_dir,
        }
    }

    /// A host with every section in it: volumes in an object store reached from the guest over
    /// NBD, artifacts and exports in S3, TLS behind an edge that presents a client certificate,
    /// raw ports for a relay, a metrics page. `deploy/config.example.toml` is this, rendered.
    ///
    /// Written out field by field rather than as changes to [`Self::starter`], so that a section
    /// added to this daemon has to be decided on here — and an example that showed every section
    /// but the newest would otherwise be exactly what nobody noticed.
    pub fn example() -> Self {
        Self {
            state_dir: PathBuf::from("/var/lib/nibrunner"),
            runtime_dir: PathBuf::from("/run/nibrunner"),
            snapshot_dir: PathBuf::from("/data/nibrunner-vm"),
            guest_image_dir: PathBuf::from("/var/lib/nibrunner/guest"),
            firecracker_dir: PathBuf::from("/run/nibrunner/firecracker"),
            desired_state_file: PathBuf::from("/var/lib/nibrunner/desired.json"),
            api_socket: PathBuf::from("/run/nibrunner/nibrunner.sock"),
            versions_file: PathBuf::from("/var/lib/nibrunner/versions.json"),
            artifact_store_url: "s3://nibrunner-artifacts-eu-west-2-123456789012/artifacts".to_string(),
            storage_prefix: "hetzner-1".to_string(),
            volumes: VolumeBackend::Zerofs(Box::new(ZerofsSettings {
                binary: PathBuf::from("/opt/nibrunner/bin/zerofs"),
                config_file: PathBuf::from("/etc/zerofs/config.toml"),
                mount_path: PathBuf::from("/mnt/zerofs"),
                nbd_socket_path: PathBuf::from("/run/zerofs/nbd.sock"),
                ninep_socket_path: PathBuf::from("/run/zerofs/9p.sock"),
                rpc_socket_path: PathBuf::from("/run/zerofs/rpc.sock"),
                storage_url: "s3://nibrunner-filesystems-eu-west-2-123456789012/hetzner-1".to_string(),
                cache_dir: PathBuf::from("/data/zerofs"),
                cache_disk_mib: 200 * MEBIBYTES_PER_GIBIBYTE,
                cache_memory_mib: 2 * MEBIBYTES_PER_GIBIBYTE,
                checkpoint_runtime_dir: PathBuf::from("/run/zerofs-checkpoint"),
                checkpoint_config_file: PathBuf::from("/etc/zerofs/checkpoint.toml"),
                checkpoint_cache_dir: PathBuf::from("/data/zerofs-checkpoint"),
            })),
            denied_egress_addresses_v4: vec![],
            denied_egress_addresses_v6: vec![],
            proxy: ProxyConfig {
                http: Some(HttpListener {
                    listen_address: IpAddr::from([0, 0, 0, 0]),
                    port: 443,
                    tls: Some(TlsMaterial {
                        certificate: PathBuf::from("/etc/nibrunner/tls/origin.crt"),
                        key: PathBuf::from("/etc/nibrunner/tls/origin.key"),
                        client_ca: Some(PathBuf::from("/etc/nibrunner/tls/origin-pull-ca.pem")),
                    }),
                }),
                raw: Some(RawPorts {
                    listen_address: IpAddr::from([10, 0, 5, 18]),
                    max_ports_per_guest: 1,
                }),
            },
            metrics: Some(MetricsConfig {
                port: 9100,
                listen_address: IpAddr::from([127, 0, 0, 1]),
            }),
            export_store_url: "s3://nibrunner-exports-eu-west-2-123456789012/exports".to_string(),
            export_staging_dir: PathBuf::from("/var/lib/nibrunner/exports"),
        }
    }

    /// This configuration as the file [`Self::from_toml`] reads. What `install` writes a host that
    /// has none, and what `deploy/config.example.toml` is written from.
    pub fn to_toml(&self) -> String {
        toml::to_string(&self.to_document())
            .expect("every key here is a string, an integer, or a table of them")
    }

    /// [`Self::from_document`] run backwards. Every key is required on the way in and every
    /// unknown one refused, so a key written here that is not read there, or read there that is
    /// not written here, fails the round trip by name rather than drifting.
    fn to_document(&self) -> file::ConfigFile {
        let text = |path: &std::path::Path| Some(path.display().to_string());
        file::ConfigFile {
            paths: Some(file::Paths {
                state_dir: text(&self.state_dir),
                runtime_dir: text(&self.runtime_dir),
                snapshot_dir: text(&self.snapshot_dir),
                guest_image_dir: text(&self.guest_image_dir),
                desired_state_file: text(&self.desired_state_file),
                api_socket: text(&self.api_socket),
                versions_file: text(&self.versions_file),
            }),
            artifacts: Some(file::Artifacts {
                store_url: Some(self.artifact_store_url.clone()),
            }),
            volumes: Some(file::Volumes {
                backend: Some(self.volumes.as_str().to_string()),
                storage_prefix: Some(self.storage_prefix.clone()),
                zerofs: self.volumes.zerofs().map(|settings| file::Zerofs {
                    binary: text(&settings.binary),
                    config_file: text(&settings.config_file),
                    mount_path: text(&settings.mount_path),
                    nbd_socket_path: text(&settings.nbd_socket_path),
                    ninep_socket_path: text(&settings.ninep_socket_path),
                    rpc_socket_path: text(&settings.rpc_socket_path),
                    storage_url: Some(settings.storage_url.clone()),
                    cache_dir: text(&settings.cache_dir),
                    cache_disk_gib: Some(settings.cache_disk_mib / MEBIBYTES_PER_GIBIBYTE),
                    cache_memory_gib: Some(settings.cache_memory_mib / MEBIBYTES_PER_GIBIBYTE),
                    checkpoint_runtime_dir: text(&settings.checkpoint_runtime_dir),
                    checkpoint_config_file: text(&settings.checkpoint_config_file),
                    checkpoint_cache_dir: text(&settings.checkpoint_cache_dir),
                }),
            }),
            exports: Some(file::Exports {
                store_url: Some(self.export_store_url.clone()),
                staging_dir: text(&self.export_staging_dir),
            }),
            network: Some(file::Network {
                denied_egress_addresses_v4: Some(self.denied_egress_addresses_v4.clone()),
                denied_egress_addresses_v6: Some(self.denied_egress_addresses_v6.clone()),
            }),
            // Left out rather than written as an empty `[proxy]`: both read back the same.
            proxy: (self.proxy != ProxyConfig::default()).then(|| file::Proxy {
                http: self.proxy.http.as_ref().map(|http| file::Http {
                    listen_address: Some(http.listen_address.to_string()),
                    port: Some(http.port),
                    tls: http.tls.as_ref().map(|tls| file::Tls {
                        certificate: text(&tls.certificate),
                        key: text(&tls.key),
                        client_ca: tls.client_ca.as_deref().map(|certificate| file::ClientCa {
                            certificate: text(certificate),
                        }),
                    }),
                }),
                raw: self.proxy.raw.as_ref().map(|raw| file::Raw {
                    listen_address: Some(raw.listen_address.to_string()),
                    max_ports_per_guest: Some(raw.max_ports_per_guest),
                }),
            }),
            metrics: self.metrics.as_ref().map(|metrics| file::Metrics {
                port: Some(metrics.port),
                listen_address: Some(metrics.listen_address.to_string()),
            }),
        }
    }
}

fn bind_address(field: &str, value: &Option<String>) -> Result<IpAddr, ConfigError> {
    required_str(field, value)?
        .trim()
        .parse()
        .map_err(|_| ConfigError::invalid(field, "an IP address to bind"))
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
                listen_address: bind_address("proxy.http.listen_address", &http.listen_address)?,
                port: listener("proxy.http.port", required("proxy.http.port", http.port)?)?,
                tls: http
                    .tls
                    .as_ref()
                    .map(|tls| {
                        Ok::<_, ConfigError>(TlsMaterial {
                            certificate: path_key("proxy.http.tls.certificate", &tls.certificate)?,
                            key: path_key("proxy.http.tls.key", &tls.key)?,
                            client_ca: tls
                                .client_ca
                                .as_ref()
                                .map(|pool| {
                                    path_key("proxy.http.tls.client_ca.certificate", &pool.certificate)
                                })
                                .transpose()?,
                        })
                    })
                    .transpose()?,
            })
        })
        .transpose()?;
    let raw = document
        .raw
        .as_ref()
        .map(|raw| {
            let named = required("proxy.raw.max_ports_per_guest", raw.max_ports_per_guest)?;
            if named == 0 || named > MAX_RAW_PORTS {
                return Err(ConfigError::invalid(
                    "proxy.raw.max_ports_per_guest",
                    format!(
                        "between 1 and {MAX_RAW_PORTS}, which is what a slot reserves beside the HTTP port"
                    ),
                ));
            }
            Ok(RawPorts {
                listen_address: bind_address("proxy.raw.listen_address", &raw.listen_address)?,
                max_ports_per_guest: named,
            })
        })
        .transpose()?;
    Ok(ProxyConfig { http, raw })
}

/// What a slot has left over for an app once its HTTP port is taken.
pub const MAX_RAW_PORTS: usize = nft_render::PORTS_PER_SLOT as usize - 1;

fn metrics(
    document: Option<&file::Metrics>,
    proxy: &ProxyConfig,
    volumes: &VolumeBackend,
) -> Result<Option<MetricsConfig>, ConfigError> {
    let Some(document) = document else {
        return Ok(None);
    };
    let port = listener("metrics.port", required("metrics.port", document.port)?)?;
    for (named, field) in [
        (proxy.http.as_ref().map(|http| http.port), "proxy.http.port"),
        // Rendered into ZeroFS's own config by `nibrunnerd install`, so this host does hold it
        // even though no key here names it, and a second binding is a startup failure over there.
        (
            volumes.zerofs().map(|_| ZEROFS_PROMETHEUS_PORT),
            "the port ZeroFS scrapes on",
        ),
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
    Ok(Some(MetricsConfig {
        port,
        listen_address: bind_address("metrics.listen_address", &document.listen_address)?,
    }))
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

/// A cache of no size is a cache ZeroFS will not start on, and it is refused here rather than
/// there — the operator is watching this file, not that one.
fn mebibytes(field: &str, gibibytes: u64) -> Result<u64, ConfigError> {
    if gibibytes == 0 {
        return Err(ConfigError::invalid(field, "more than nothing"));
    }
    gibibytes
        .checked_mul(MEBIBYTES_PER_GIBIBYTE)
        .ok_or_else(|| ConfigError::invalid(field, "a size this machine could hold"))
}

fn listener(field: &str, port: u16) -> Result<u16, ConfigError> {
    if port == 0 {
        return Err(ConfigError::invalid(
            field,
            "a port, and 0 is the kernel picking one",
        ));
    }
    let (base, end) = nft_render::reserved_port_range();
    if (base..=end).contains(&port) {
        return Err(ConfigError::invalid(
            field,
            format!("free, because {base}-{end} is what a slot takes for an app's loopback port"),
        ));
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
denied_egress_addresses_v4 = []
denied_egress_addresses_v6 = []
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

    /// Every listener binds somewhere, so a document naming one names that too. Each on an
    /// address of its own, because each faces a different machine.
    fn bound(listeners: &str) -> String {
        listeners
            .replace("[proxy.http]\n", "[proxy.http]\nlisten_address = \"0.0.0.0\"\n")
            .replace("[proxy.raw]\n", "[proxy.raw]\nlisten_address = \"10.0.5.18\"\n")
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
            "network.denied_egress_addresses_v4",
            "network.denied_egress_addresses_v6",
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
            with(&bound(
                "[proxy.http]\nport = 443\n\n[proxy.http.tls]\ncertificate = \"  /tls/origin.crt  \"\nkey = \"/tls/origin.key\"\n"
            ))
            .proxy
            .http
            .unwrap()
            .tls
            .unwrap()
            .certificate,
            PathBuf::from("/tls/origin.crt")
        );
    }

    #[test]
    fn a_proxy_port_a_slot_would_take_is_refused() {
        let message = refused(&document(&[], &bound("[proxy.http]\nport = 21000\n")));
        assert!(message.contains("proxy.http.port"), "{message}");
        assert!(message.contains("21000"), "{message}");
        assert!(message.contains("28999"), "{message}");
        // The whole stride is reserved, not just the port each slot's first app answers on.
        assert!(refused(&document(&[], &bound("[proxy.http]\nport = 23000\n"))).contains("proxy.http.port"));
        assert_eq!(
            with(&bound("[proxy.http]\nport = 29000\n"))
                .proxy
                .http
                .unwrap()
                .port,
            29000
        );
    }

    #[test]
    fn a_port_the_kernel_would_pick_is_not_a_port_this_host_can_be_found_on() {
        let message = refused(&document(&[], &bound("[proxy.http]\nport = 0\n")));
        assert!(message.contains("proxy.http.port"), "{message}");
        assert!(message.contains("kernel picking one"), "{message}");
    }

    #[test]
    fn a_listener_that_binds_nowhere_is_refused_rather_than_bound_everywhere() {
        let message = refused(&document(&[], "[proxy.http]\nport = 8080\n"));
        assert!(message.contains("proxy.http.listen_address"), "{message}");
        let raw = refused(&document(&[], "[proxy.raw]\nmax_ports_per_guest = 1\n"));
        assert!(raw.contains("proxy.raw.listen_address"), "{raw}");
        // A host that offers no way in is not asked where it would have bound one.
        assert_eq!(parsed(&whole()).proxy, ProxyConfig::default());
    }

    #[test]
    fn each_listener_binds_where_it_says_and_not_where_the_other_does() {
        let both = with(&bound(
            "[proxy.http]\nport = 8080\n\n[proxy.raw]\nmax_ports_per_guest = 1\n",
        ));
        assert_eq!(
            both.proxy.http.unwrap().listen_address,
            IpAddr::from([0, 0, 0, 0]),
            "HTTP faces the edge"
        );
        assert_eq!(
            both.proxy.raw.unwrap().listen_address,
            IpAddr::from([10, 0, 5, 18]),
            "raw ports face the relay"
        );
    }

    #[test]
    fn tls_is_a_property_of_the_one_listener_rather_than_a_second_one() {
        let plain = with(&bound("[proxy.http]\nport = 8080\n"));
        let listener = plain.proxy.http.unwrap();
        assert_eq!(listener.port, 8080);
        assert_eq!(listener.tls, None, "no material is plain HTTP, not a refusal");

        let secure = with(&bound(
            "[proxy.http]\nport = 8443\n\n[proxy.http.tls]\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n",
        ));
        assert_eq!(secure.proxy.http.unwrap().port, 8443);

        // The listener it would have belonged to is gone, so there is nowhere to write a second.
        let second = refused(&document(&[], &bound("[proxy.https]\nport = 8443\n")));
        assert!(second.contains("https"), "{second}");
    }

    #[test]
    fn a_listener_is_named_whole_or_not_at_all() {
        let message = refused(&document(
            &[],
            &bound("[proxy.http]\nport = 8443\n\n[proxy.http.tls]\n"),
        ));
        assert!(message.contains("proxy.http.tls.certificate"), "{message}");
        let half = refused(&document(
            &[],
            &bound("[proxy.http]\nport = 8443\n\n[proxy.http.tls]\ncertificate = \"/tls/origin.crt\"\n"),
        ));
        assert!(half.contains("proxy.http.tls.key"), "{half}");
        let pool = refused(&document(
            &[],
            &bound("[proxy.http]\nport = 8443\n\n[proxy.http.tls]\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n\n[proxy.http.tls.client_ca]\n"),
        ));
        assert!(pool.contains("proxy.http.tls.client_ca.certificate"), "{pool}");
    }

    #[test]
    fn a_trust_pool_is_only_reachable_where_something_serves_tls_to_check_a_caller_against_it() {
        let served = with(&bound("[proxy.http]\nport = 8443\n\n[proxy.http.tls]\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n\n[proxy.http.tls.client_ca]\ncertificate = \"/tls/ca.pem\"\n"));
        assert_eq!(
            served.proxy.http.unwrap().tls.unwrap().client_ca,
            Some(PathBuf::from("/tls/ca.pem"))
        );
        let open = with(&bound(
            "[proxy.http]\nport = 8443\n\n[proxy.http.tls]\ncertificate = \"/tls/c\"\nkey = \"/tls/k\"\n",
        ));
        assert_eq!(open.proxy.http.unwrap().tls.unwrap().client_ca, None);
    }

    #[test]
    fn how_many_ports_an_app_may_name_is_the_hosts_to_say_and_is_bounded_by_the_slot() {
        assert_eq!(
            with(&bound("[proxy.raw]\nmax_ports_per_guest = 1\n"))
                .proxy
                .raw
                .unwrap()
                .max_ports_per_guest,
            1
        );
        for refused_count in ["0", &(MAX_RAW_PORTS + 1).to_string()] {
            let message = refused(&document(
                &[],
                &bound(&format!("[proxy.raw]\nmax_ports_per_guest = {refused_count}\n")),
            ));
            assert!(message.contains("proxy.raw.max_ports_per_guest"), "{message}");
        }
        assert_eq!(
            with(&bound(&format!(
                "[proxy.raw]\nmax_ports_per_guest = {MAX_RAW_PORTS}\n"
            )))
            .proxy
            .raw
            .unwrap()
            .max_ports_per_guest,
            MAX_RAW_PORTS
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
            &bound("[proxy.http]\nport = 9100\n\n[metrics]\nport = 9100\nlisten_address = \"127.0.0.1\"\n"),
        ));
        assert!(clash.contains("proxy.http.port"), "{clash}");
    }

    #[test]
    fn a_range_that_nft_would_reject_is_refused_before_the_ruleset_is_rendered() {
        let bad = |value: &str| refused(&document(&[("network.denied_egress_addresses_v4", value)], ""));
        assert!(bad("[\"172.31.0.0\"]").contains("prefix length"));
        assert!(bad("[\"172.31.0.0/33\"]").contains("wider"));
        assert!(bad("[\"fd00::/8\"]").contains("IPv4"));
        assert!(bad("[\"10.0.0.0/eight\"]").contains("not a prefix length"));
        assert!(refused(&document(
            &[("network.denied_egress_addresses_v6", "[\"172.31.0.0/16\"]")],
            ""
        ))
        .contains("IPv6"));
        assert!(refused(&document(
            &[("network.denied_egress_addresses_v6", "[\"fd00::/129\"]")],
            ""
        ))
        .contains("wider"));
        assert_eq!(
            parsed(&document(
                &[("network.denied_egress_addresses_v4", "[\"172.31.0.0/16\"]")],
                ""
            ))
            .denied_egress_addresses_v4,
            vec!["172.31.0.0/16".to_string()]
        );
        assert_eq!(
            parsed(&document(
                &[("network.denied_egress_addresses_v6", "[\" fd00::/8 \"]")],
                ""
            ))
            .denied_egress_addresses_v6,
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
ninep_socket_path = "/run/zerofs/9p.sock"
rpc_socket_path = "/run/zerofs/rpc.sock"
storage_url = "s3://filesystems-one/host-1"
cache_dir = "/data/zerofs"
cache_disk_gib = 70
cache_memory_gib = 2
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

    // The cache this host reserves against is read back out of the file `install` renders, and
    // `cache_gigabytes` truncates to whole gibibytes — so a fraction here would hold back a number
    // ZeroFS is not taking. Stating it in gibibytes is what makes that unspellable.
    #[test]
    fn a_cache_is_stated_in_the_unit_it_can_be_read_back_in() {
        let config = parsed(&document(&[("volumes.backend", "\"zerofs\"")], ZEROFS));
        let zerofs = config.volumes.zerofs().unwrap();
        assert_eq!(zerofs.cache_disk_mib, 70 * 1024);
        assert_eq!(zerofs.cache_memory_mib, 2 * 1024);

        for empty in ["cache_disk_gib", "cache_memory_gib"] {
            let zeroed = ZEROFS
                .replace(&format!("{empty} = 70"), &format!("{empty} = 0"))
                .replace(&format!("{empty} = 2"), &format!("{empty} = 0"));
            let message = refused(&document(&[("volumes.backend", "\"zerofs\"")], &zeroed));
            assert!(message.contains(empty), "{message}");
        }
    }

    // ZeroFS binds this itself, and a second binding is a startup failure over there rather than
    // a refusal here — so a host that serves both is stopped while an operator is still watching.
    #[test]
    fn a_metrics_port_zerofs_already_holds_is_refused() {
        let clash =
            format!("{ZEROFS}\n[metrics]\nport = {ZEROFS_PROMETHEUS_PORT}\nlisten_address = \"127.0.0.1\"\n");
        let message = refused(&document(&[("volumes.backend", "\"zerofs\"")], &clash));
        assert!(message.contains("metrics.port"), "{message}");

        let local = format!("[metrics]\nport = {ZEROFS_PROMETHEUS_PORT}\nlisten_address = \"127.0.0.1\"\n");
        assert_eq!(
            parsed(&document(&[], &local)).metrics.unwrap().port,
            ZEROFS_PROMETHEUS_PORT,
            "a local-file host runs no zerofs, so nothing holds that port"
        );
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

    // What `install` writes a host that has none is the smallest document every test here starts
    // from, plus the one listener a starting point has to serve on — so the two are held to be one
    // text.
    #[test]
    fn the_configuration_this_binary_carries_is_the_smallest_document_with_a_listener_rendered() {
        assert_eq!(
            HostConfig::starter().to_toml(),
            document(&[], &bound("\n[proxy.http]\nport = 80\n"))
        );
    }

    // Reading is writing run backwards, key for key. Every key is required on the way in and every
    // unknown one refused, so a key rendered that is not read, or read that is not rendered, fails
    // here by name — which is what lets the example this repository ships be written from code.
    #[test]
    fn a_configuration_rendered_is_the_configuration_read_back() {
        for config in [
            HostConfig::starter(),
            HostConfig::example(),
            HostConfig::under(Path::new("/srv/one-host")),
        ] {
            let rendered = config.to_toml();
            assert_eq!(parsed(&rendered), config, "{rendered}");
        }
    }

    // Every section this daemon reads is in the example, or it is not an example of every section.
    #[test]
    fn the_example_names_every_section_there_is() {
        let config = HostConfig::example();
        assert!(config.volumes.zerofs().is_some());
        assert!(config
            .proxy
            .http
            .as_ref()
            .and_then(|http| http.tls.as_ref())
            .and_then(|tls| tls.client_ca.as_ref())
            .is_some());
        assert!(config.proxy.raw.is_some());
        assert!(config.metrics.is_some());
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
                ("network.denied_egress_addresses_v4", "[\"172.31.0.0/16\"]"),
            ],
            r#"[proxy.http]
listen_address = "0.0.0.0"
port = 443

[proxy.http.tls]
certificate = "/etc/nibrunner/origin.crt"
key = "/etc/nibrunner/origin.key"

[proxy.http.tls.client_ca]
certificate = "/etc/nibrunner/origin-pull-ca.pem"

[proxy.raw]
listen_address = "10.0.5.18"
max_ports_per_guest = 1

[metrics]
port = 9100
listen_address = "127.0.0.1"
"#,
        ));
        assert_eq!(config.state_dir, PathBuf::from("/srv/nibrunner"));
        assert_eq!(config.snapshot_dir, PathBuf::from("/mnt/cache/snapshots"));
        assert_eq!(config.artifact_store_url, "s3://nibrun-artifacts/prod");
        assert_eq!(
            config.denied_egress_addresses_v4,
            vec!["172.31.0.0/16".to_string()]
        );
        assert_eq!(
            config.proxy.http,
            Some(HttpListener {
                listen_address: IpAddr::from([0, 0, 0, 0]),
                port: 443,
                tls: Some(TlsMaterial {
                    certificate: PathBuf::from("/etc/nibrunner/origin.crt"),
                    key: PathBuf::from("/etc/nibrunner/origin.key"),
                    client_ca: Some(PathBuf::from("/etc/nibrunner/origin-pull-ca.pem")),
                }),
            })
        );
        assert_eq!(
            config.proxy.raw,
            Some(RawPorts {
                listen_address: IpAddr::from([10, 0, 5, 18]),
                max_ports_per_guest: 1,
            })
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
