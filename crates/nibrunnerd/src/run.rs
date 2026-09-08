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
use crate::adapters::vm::manager::{verify_guest_image, VmManager};
use crate::adapters::vm::process::{extract_firecracker, VmProcesses, FIRECRACKER_VERSION};
use crate::adapters::volumes::local_file::LocalFileVolumes;
use crate::adapters::volumes::zerofs::{ZerofsFilesystem, ZerofsVolumes};
use crate::config::HostConfig;
use crate::desired::DesiredStateCache;
use crate::domain::exports::reader::CheckpointServers;
use crate::domain::report::capacity::{guest_memory_mib, read_host_memory_mib};
use crate::domain::waker::AppWaker;
use crate::host::Host;
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

    let guest_image_version = verify_guest_image(&config.guest_image_dir)
        .map_err(|error| StartupError::Unusable(error.message()))?;
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
    let volumes: Arc<dyn crate::adapters::volumes::VolumeBackend> = match config.volumes.zerofs() {
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
        guest_image_version: guest_image_version.clone(),
        firecracker,
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
    let checkpoint_servers = config.volumes.zerofs().map(|settings| CheckpointServers {
        ready_timeout: crate::domain::exports::reader::DEFAULT_READY_TIMEOUT,
        binary: settings.binary.clone(),
        config_file: settings.checkpoint_config_file.clone(),
        runtime_dir: settings.checkpoint_runtime_dir.clone(),
        cache_dir: settings.checkpoint_cache_dir.clone(),
    });

    let host = Arc::new(Host {
        guest_memory_mib: guest_memory_mib(read_host_memory_mib(), volumes.reserved_cache().memory_mib()),
        guest_image_version,
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
impl crate::ports::Waker for DeferredWaker {
    async fn wake(&self, app_id: &protocol::AppId) -> Result<(), crate::ports::WakeRefusal> {
        match self.waker.get() {
            Some(waker) => waker.wake(app_id).await,
            None => Err(crate::ports::WakeRefusal::Failed {
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
        crate::domain::report::versions::compiled_versions(FIRECRACKER_VERSION, &host.guest_image_version)
    })
}

pub fn serve_proxy(host: &Arc<Host>) {
    if let Some(http) = &host.config.proxy.http {
        let (router, port) = (host.router.clone(), http.port);
        tokio::spawn(async move {
            let address = SocketAddr::from(([0, 0, 0, 0], port));
            if let Err(error) = router::serve_http(router, address).await {
                tracing::error!(%error, "the proxy could not listen");
            }
        });
    }
    let Some(https) = host.config.proxy.https.clone() else {
        return;
    };
    let router = host.router.clone();
    tokio::spawn(async move {
        let address = SocketAddr::from(([0, 0, 0, 0], https.port));
        if let Err(error) = router::serve_https(
            router,
            address,
            &https.certificate,
            &https.key,
            https.client_ca.as_deref(),
        )
        .await
        {
            tracing::error!(%error, "the proxy could not listen for TLS");
        }
    });
}

// The scrape surface. It is a second way to read what `reported.json` already says rather than a
// second account of it: the page is rendered from the same builder the status loop writes with, so
// a scraper and the file can never disagree. Nothing here is an input — there is no route that
// changes anything, which is what keeps "nothing may tell this daemon what to do except by writing
// that document" true of a daemon that now answers a connection.
pub fn serve_metrics(host: &Arc<Host>) {
    let Some(metrics) = &host.config.metrics else {
        return;
    };
    let address = SocketAddr::new(metrics.listen_address, metrics.port);
    let host = host.clone();
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::error!(%error, %address, "metrics could not be served");
                return;
            }
        };
        tracing::info!(%address, "metrics are being served");
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let host = host.clone();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                        let host = host.clone();
                        async move {
                            Ok::<_, std::convert::Infallible>(
                                answer_scrape(&host, request.uri().path()).await,
                            )
                        }
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

async fn answer_scrape(host: &Arc<Host>, path: &str) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
    use http_body_util::Full;
    if path != "/metrics" {
        return hyper::Response::builder()
            .status(hyper::StatusCode::NOT_FOUND)
            .body(Full::new(bytes::Bytes::from_static(
                b"Metrics are at /metrics.\n",
            )))
            .expect("a constant response is always buildable");
    }
    let report = crate::domain::report::writer::build(host, host_versions(host)).await;
    let page = crate::domain::metrics::render(&report, &host.router.metrics());
    hyper::Response::builder()
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(Full::new(bytes::Bytes::from(page)))
        .expect("a rendered page is always buildable")
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
