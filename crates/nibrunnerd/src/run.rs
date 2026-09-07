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
use crate::domain::exports::reader::CheckpointServers;
use crate::domain::report::capacity::{guest_memory_mib, read_host_memory_mib};
use crate::host::Host;
use crate::services::waker_service::AppWaker;
use crate::state::HostState;

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("{0}")]
    Config(String),
    #[error("this host is not one a microVM can run on: {0}")]
    Unusable(String),
}

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
    let repositories = crate::repositories::Repositories::sqlite(
        crate::domain::store::open(&config.state_db_file())
            .await
            .map_err(|error| StartupError::Unusable(error.message()))?,
    );
    if let Err(error) = crate::domain::store::import::import_documents(&repositories, &config).await {
        tracing::warn!(error = %error.message(), "what an earlier daemon wrote could not be carried over");
    }
    let allocator = Arc::new(Mutex::new(SlotAllocator::empty()));

    let storage_prefix = ObjectKey::parse(&config.storage_prefix)
        .map_err(|_| StartupError::Config("volumes.storage_prefix is not a key".into()))?;
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

    let waker_slot: Arc<tokio::sync::OnceCell<Arc<AppWaker>>> = Arc::new(tokio::sync::OnceCell::new());
    let activator = AppActivator::new(
        state.clone(),
        Arc::new(DeferredWaker {
            waker: waker_slot.clone(),
        }),
    );

    let exports: Arc<dyn crate::domain::exports::store::ExportStore> = Arc::new(
        crate::domain::exports::store::ObjectExportStore::open(&config.export_store_url)
            .map_err(|error| StartupError::Config(error.message()))?,
    );
    let checkpoint_servers = config.zerofs.as_ref().map(|settings| CheckpointServers {
        ready_timeout: crate::domain::exports::reader::DEFAULT_READY_TIMEOUT,
        binary: settings.binary.clone(),
        config_file: settings.checkpoint_config_file.clone(),
        runtime_dir: settings.checkpoint_runtime_dir.clone(),
        cache_dir: settings.checkpoint_cache_dir.clone(),
    });

    let host = Arc::new(Host {
        guest_memory_mib: guest_memory_mib(read_host_memory_mib(), volumes.reserved_cache().memory_mib()),
        state,
        allocator: allocator.clone(),
        repositories,
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

#[cfg(not(target_os = "linux"))]
fn open_network() -> Result<Arc<dyn HostNetwork>, StartupError> {
    Err(StartupError::Unusable(
        "a microVM needs a Linux kernel with /dev/kvm".into(),
    ))
}

pub fn host_versions(host: &Host) -> HostVersions {
    crate::domain::report::versions::read_host_versions(&host.config.versions_file).unwrap_or_else(|_| {
        crate::domain::report::versions::compiled_versions(
            FIRECRACKER_VERSION,
            &read_guest_image_version(&host.config.guest_image_dir),
        )
    })
}

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
    use crate::controllers::converge_controller::ConvergeController;
    use crate::controllers::Controller;
    use crate::services::reconcile_service::HostReconciler;
    use crate::test_support::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_document_written_to_the_watched_file_is_converged_on() {
        let host = test_host().await;
        let desired = desired_state(|state| {
            state.volumes = vec![desired_volume(|_| {})];
            state.instances = vec![desired_instance(|_| {})];
        });
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired).unwrap();

        let controller = ConvergeController::new(host.arc().clone(), HostReconciler::new(host.arc().clone()));
        let converging = tokio::spawn(async move { controller.run().await });
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

    #[tokio::test]
    async fn a_missing_document_is_the_ordinary_state_of_a_fresh_host() {
        let host = test_host().await;
        let controller = ConvergeController::new(host.arc().clone(), HostReconciler::new(host.arc().clone()));
        let converging = tokio::spawn(async move { controller.run().await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        converging.abort();
        assert!(host.state.records().await.is_empty());
    }
}
