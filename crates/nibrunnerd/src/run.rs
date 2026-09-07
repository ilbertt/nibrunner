//! Building the host, and the three loops it runs.

use std::net::SocketAddr;
use std::sync::Arc;

use protocol::{HostVersions, ObjectKey};
use tokio::sync::Mutex;

use crate::adapters::artifact_store::ObjectArtifactStore;
use crate::adapters::exec::HostCommands;
use crate::adapters::logs::receiver::TenantLogReceiver;
use crate::adapters::logs::FileLogSink;
use crate::adapters::net::allocator::SlotAllocator;
use crate::adapters::net::firewall::HostFirewall;
use crate::adapters::net::tap::HostNetwork;
use crate::adapters::proxy::activator::AppActivator;
use crate::adapters::proxy::{router, Router};
use crate::adapters::vm::manager::{read_guest_image_version, VmManager};
use crate::adapters::vm::process::{extract_firecracker, VmProcesses, FIRECRACKER_VERSION};
use crate::adapters::volumes::local_file::LocalFileVolumes;
use crate::adapters::volumes::zerofs::{ZerofsFilesystem, ZerofsVolumes};
use crate::config::HostConfig;
use crate::desired::DesiredStateCache;
use crate::host::Host;
use crate::services::exports::reader::CheckpointServers;
use crate::services::report::capacity::{guest_memory_mib, read_host_memory_mib};
use crate::services::waker::AppWaker;
use crate::state::HostState;

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("{0}")]
    Config(String),
    #[error("this host is not one a microVM can run on: {0}")]
    Unusable(String),
}

/// Everything a host is, built once. What fails here fails before a tenant exists, which is the
/// only place a host is allowed to refuse to be one.
pub async fn build_host(config: HostConfig) -> Result<Arc<Host>, StartupError> {
    for directory in [&config.state_dir, &config.runtime_dir, &config.snapshot_dir] {
        crate::json_store::make_directory(directory, 0o700).map_err(|error| {
            StartupError::Config(format!("{} could not be made: {error}", directory.display()))
        })?;
    }

    let firecracker = extract_firecracker(&config.firecracker_dir)
        .map_err(|error| StartupError::Unusable(error.to_string()))?;
    let commands: Arc<dyn crate::ports::CommandRunner> = Arc::new(HostCommands);
    let state = HostState::shared();
    // Opened before anything reads it, and migrated on the way: a host that has just been given
    // the binary has no database, and one given a newer binary has an older schema.
    let store = crate::repositories::open(&config.state_db_file())
        .await
        .map_err(|error| StartupError::Unusable(error.message()))?;
    // Whatever an older daemon on this host left in documents, once. A host upgraded with apps on
    // it must not re-allocate their slots — that would move a tenant's port under a live client.
    if let Err(error) = crate::repositories::import_documents(&store, &config).await {
        tracing::warn!(error = %error.message(), "what an earlier daemon wrote could not be carried over");
    }
    // Empty here and filled by `host.load()`, which is the one place that reads the notes.
    let allocator = Arc::new(Mutex::new(SlotAllocator::empty()));

    let storage_prefix = ObjectKey::parse(&config.storage_prefix)
        .map_err(|_| StartupError::Config("volumes.storage_prefix is not a key".into()))?;
    // Chosen once, here, rather than asked per volume: what a volume is made of is a property of
    // the host, and a daemon that could answer differently on two passes is one whose tenant finds
    // its disk somewhere else.
    let volumes: Arc<dyn crate::adapters::volumes::VolumeBackend> = match &config.zerofs {
        None => Arc::new(LocalFileVolumes::new(
            config.volumes_dir(),
            storage_prefix,
            commands.clone(),
        )),
        Some(settings) => Arc::new(ZerofsVolumes::new(
            ZerofsFilesystem {
                storage_prefix,
                mount_path: settings.mount_path.clone(),
                nbd_socket_path: settings.nbd_socket_path.clone(),
                checkpoint_runtime_dir: settings.checkpoint_runtime_dir.clone(),
                binary: settings.binary.clone(),
                config_file: settings.config_file.clone(),
            },
            allocator.clone(),
            commands.clone(),
        )),
    };
    let artifacts = Arc::new(
        ObjectArtifactStore::open(&config.artifact_store_url)
            .map_err(|error| StartupError::Config(error.message()))?,
    );
    let network = open_network()?;
    let logs = TenantLogReceiver::new();
    let sink = Arc::new(FileLogSink::new(config.logs_dir()));

    let vms = Arc::new(VmManager {
        vm_dir: config.vm_dir(),
        snapshot_dir: config.snapshot_dir.clone(),
        guest_image_dir: config.guest_image_dir.clone(),
        guest_image_version: read_guest_image_version(&config.guest_image_dir),
        firecracker,
        public_ipv4: config.port_relay_public_ipv4.clone(),
        processes: VmProcesses::new(config.runtime_dir.clone()),
        network,
        volumes: volumes.clone(),
        logs,
        sink,
        state: state.clone(),
    });

    // Built in two steps because the waker needs the host and the host needs the activator the
    // waker is behind: the activator is given a waker that holds the host once the host exists.
    let waker_slot: Arc<tokio::sync::OnceCell<Arc<AppWaker>>> = Arc::new(tokio::sync::OnceCell::new());
    let activator = AppActivator::new(
        state.clone(),
        Arc::new(DeferredWaker {
            waker: waker_slot.clone(),
        }),
    );

    let exports: Arc<dyn crate::services::exports::store::ExportStore> = Arc::new(
        crate::services::exports::store::ObjectExportStore::open(&config.export_store_url)
            .map_err(|error| StartupError::Config(error.message()))?,
    );
    // Only where a volume is something a checkpoint can be cut from.
    let checkpoint_servers = config.zerofs.as_ref().map(|settings| CheckpointServers {
        binary: settings.binary.clone(),
        config_file: settings.checkpoint_config_file.clone(),
        runtime_dir: settings.checkpoint_runtime_dir.clone(),
        cache_dir: settings.checkpoint_cache_dir.clone(),
    });

    let host = Arc::new(Host {
        guest_memory_mib: guest_memory_mib(read_host_memory_mib(), volumes.reserved_cache().memory_mib()),
        state,
        allocator: allocator.clone(),
        store,
        exports,
        checkpoint_servers,
        nbd: crate::adapters::volumes::nbd::NbdDevices::new(commands.clone()),
        commands: commands.clone(),
        cache: Mutex::new(DesiredStateCache::new()),
        vms,
        volumes,
        artifacts,
        firewall: Arc::new(HostFirewall::new(commands)),
        router: Router::new(),
        activator,
        config,
    });
    let _ = waker_slot.set(AppWaker::new(host.clone()));
    Ok(host)
}

/// The activator holds this rather than the waker itself, because the waker holds the host and
/// the host holds the activator. Nothing is ever asked of it before the host exists: the only
/// thing that calls it is a request on a port the reconcile has not bound yet.
struct DeferredWaker {
    waker: Arc<tokio::sync::OnceCell<Arc<AppWaker>>>,
}

#[async_trait::async_trait]
impl crate::adapters::proxy::activator::Waker for DeferredWaker {
    async fn wake(
        &self,
        app_id: &protocol::AppId,
    ) -> Result<(), crate::adapters::proxy::activator::WakeRefusal> {
        match self.waker.get() {
            Some(waker) => waker.wake(app_id).await,
            None => Err(crate::adapters::proxy::activator::WakeRefusal::Failed {
                reason: "this host is still starting".into(),
            }),
        }
    }
}

#[cfg(target_os = "linux")]
fn open_network() -> Result<Arc<dyn HostNetwork>, StartupError> {
    crate::adapters::net::tap::KernelNetwork::open()
        .map(|network| Arc::new(network) as Arc<dyn HostNetwork>)
        .map_err(|error| StartupError::Unusable(error.message()))
}

/// Off Linux there is no tap to make and no guest to put behind one. The daemon still builds, so
/// every test but the ones that boot runs anywhere, and it refuses at startup rather than at the
/// first deploy.
#[cfg(not(target_os = "linux"))]
fn open_network() -> Result<Arc<dyn HostNetwork>, StartupError> {
    Err(StartupError::Unusable(
        "a microVM needs a Linux kernel with /dev/kvm".into(),
    ))
}

pub fn host_versions(host: &Host) -> HostVersions {
    crate::services::report::versions::read_host_versions(&host.config.versions_file).unwrap_or_else(|_| {
        crate::services::report::versions::compiled_versions(
            FIRECRACKER_VERSION,
            &read_guest_image_version(&host.config.guest_image_dir),
        )
    })
}

/// The edge, where a host has one. A daemon with no ports configured serves nothing itself, which
/// is what a host behind a proxy of its own wants.
pub fn serve_proxy(host: &Arc<Host>) {
    if let Some(port) = host.config.proxy_http_port {
        let router = host.router.clone();
        tokio::spawn(async move {
            let address = SocketAddr::from(([0, 0, 0, 0], port));
            if let Err(error) = router::serve_http(router, address).await {
                tracing::error!(%error, "the proxy could not listen");
            }
        });
    }
    let Some(port) = host.config.proxy_https_port else {
        return;
    };
    let Some((certificate, key)) = host.config.tls_material() else {
        tracing::warn!("an HTTPS port was named with no certificate beside it, so nothing serves TLS");
        return;
    };
    let (router, certificate, key) = (host.router.clone(), certificate.to_path_buf(), key.to_path_buf());
    tokio::spawn(async move {
        let address = SocketAddr::from(([0, 0, 0, 0], port));
        if let Err(error) = router::serve_https(router, address, &certificate, &key).await {
            tracing::error!(%error, "the proxy could not listen for TLS");
        }
    });
}

#[cfg(test)]
mod tests {
    use crate::test_support::*;
    use std::time::Duration;

    /// The one loop that has to work before anything else does: a document appears, and the host
    /// converges on it without anybody telling it to.
    #[tokio::test]
    async fn a_document_written_to_the_watched_file_is_converged_on() {
        let host = test_host().await;
        let desired = desired_state(|state| {
            state.volumes = vec![desired_volume(|_| {})];
            state.instances = vec![desired_instance(|_| {})];
        });
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired).unwrap();

        let converging = tokio::spawn(crate::controllers::converge_loop(host.arc().clone()));
        for _ in 0..200 {
            if host.state.record(&app_id()).await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        converging.abort();

        assert!(host.state.record(&app_id()).await.is_some());
        assert_eq!(host.vms.calls(), vec![crate::ports::VmCall::Boot]);
        assert_eq!(host.cached_desired_state().await.as_ref(), Some(&desired));
    }

    /// A host with nothing on it is not a host that failed: a fresh machine has no document yet.
    #[tokio::test]
    async fn a_missing_document_is_the_ordinary_state_of_a_fresh_host() {
        let host = test_host().await;
        let converging = tokio::spawn(crate::controllers::converge_loop(host.arc().clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        converging.abort();
        assert!(host.state.records().await.is_empty());
    }
}
