//! Building the host, and the three loops it runs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use protocol::{HostVersions, ObjectKey};
use tokio::sync::Mutex;

use crate::artifact_store::ObjectArtifactStore;
use crate::config::HostConfig;
use crate::desired::{DesiredStateCache, DesiredStateWatch};
use crate::exec::HostCommands;
use crate::host::Host;
use crate::logs::receiver::TenantLogReceiver;
use crate::logs::FileLogSink;
use crate::net::allocator::SlotAllocator;
use crate::net::firewall::HostFirewall;
use crate::net::tap::HostNetwork;
use crate::proxy::activator::AppActivator;
use crate::proxy::{router, Router};
use crate::report::capacity::{guest_memory_mib, read_host_memory_mib};
use crate::state::HostState;
use crate::vm::manager::{read_guest_image_version, VmManager};
use crate::vm::process::{extract_firecracker, VmProcesses, FIRECRACKER_VERSION};
use crate::volumes::local_file::LocalFileVolumes;
use crate::waker::AppWaker;

/// One tick of the status loop on a host where nothing is settling. A probe cannot land sooner
/// than the tick that runs it, so the two share one cadence.
const STATUS_TICK: Duration = Duration::from_secs(1);
const SETTLING_TICK: Duration = Duration::from_millis(crate::health::STARTUP_PROBE_INTERVAL_MS);
/// The floor a signal cannot get under. A refresh probes every instance that is due and writes
/// the host's state to disk, so left ungated a host waking apps steadily would run it as fast as
/// the disk allows — and a host with enough on-request apps to be waking them steadily is the one
/// this whole feature is for.
const MIN_REFRESH_GAP: Duration = Duration::from_millis(250);

/// How often each guest is measured, and the window every activity reading is taken over.
/// Slower than the status tick because the counters only move at the speed a tenant is used, and
/// a sleep is not work to put in front of the health probes of every other app on the host.
const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

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
        crate::json_store::make_directory(directory, 0o700)
            .map_err(|error| StartupError::Config(format!("{} could not be made: {error}", directory.display())))?;
    }

    let firecracker = extract_firecracker(&config.firecracker_dir)
        .map_err(|error| StartupError::Unusable(error.to_string()))?;
    let commands: Arc<dyn crate::services::CommandRunner> = Arc::new(HostCommands);
    let state = HostState::shared();
    let allocator = SlotAllocator::load(&config.slots_file(), &config.slot_cursor_file())
        .map_err(|error| StartupError::Config(error.message()))?;

    let storage_prefix = ObjectKey::parse(&config.storage_prefix)
        .map_err(|_| StartupError::Config("NIBRUNNER_STORAGE_PREFIX is not a key".into()))?;
    let volumes = Arc::new(LocalFileVolumes::new(config.volumes_dir(), storage_prefix, commands.clone()));
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
    let activator = AppActivator::new(state.clone(), Arc::new(DeferredWaker { waker: waker_slot.clone() }));

    let host = Arc::new(Host {
        guest_memory_mib: guest_memory_mib(read_host_memory_mib(), 0),
        state,
        allocator: Mutex::new(allocator),
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
impl crate::proxy::activator::Waker for DeferredWaker {
    async fn wake(&self, app_id: &protocol::AppId) -> Result<(), crate::proxy::activator::WakeRefusal> {
        match self.waker.get() {
            Some(waker) => waker.wake(app_id).await,
            None => Err(crate::proxy::activator::WakeRefusal::Failed {
                reason: "this host is still starting".into(),
            }),
        }
    }
}

#[cfg(target_os = "linux")]
fn open_network() -> Result<Arc<dyn HostNetwork>, StartupError> {
    crate::net::tap::KernelNetwork::open()
        .map(|network| Arc::new(network) as Arc<dyn HostNetwork>)
        .map_err(|error| StartupError::Unusable(error.message()))
}

/// Off Linux there is no tap to make and no guest to put behind one. The daemon still builds, so
/// every test but the ones that boot runs anywhere, and it refuses at startup rather than at the
/// first deploy.
#[cfg(not(target_os = "linux"))]
fn open_network() -> Result<Arc<dyn HostNetwork>, StartupError> {
    Err(StartupError::Unusable("a microVM needs a Linux kernel with /dev/kvm".into()))
}

pub fn host_versions(host: &Host) -> HostVersions {
    crate::report::versions::read_host_versions(&host.config.versions_file).unwrap_or_else(|_| {
        crate::report::versions::compiled_versions(
            FIRECRACKER_VERSION,
            &read_guest_image_version(&host.config.guest_image_dir),
        )
    })
}

/// Converges against the last document this host was given before anything has written a new one,
/// then watches for one. A file that is not there yet is a host with nothing to run, which is the
/// ordinary state of a fresh machine rather than a failure.
pub async fn converge_loop(host: Arc<Host>) {
    let watch = DesiredStateWatch::on(&host.config.desired_state_file);
    // The cached copy first: a restart during an outage of whatever writes the file is a
    // non-event, because the host still knows what it is supposed to be running.
    if let Some(cached) = host.cached_desired_state().await {
        if host.cache.lock().await.accept(cached.clone()) {
            crate::reconcile::reconcile(&host, &cached).await;
        }
    }
    loop {
        match crate::desired::read_desired_state(&host.config.desired_state_file) {
            Ok(Some(desired)) => {
                let news = host.cache.lock().await.accept(desired.clone());
                if news {
                    let _ = crate::desired::cache_desired_state(&host.config.cached_desired_state_file(), &desired);
                    crate::reconcile::reconcile(&host, &desired).await;
                } else if host.state.snapshot().await.deferred_work {
                    // Only a document moving runs a pass, and work the last one deferred does not
                    // move it — so a volume waiting on an instance to stop would never be carried.
                    crate::reconcile::reconcile(&host, &desired).await;
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(error = %error.message(), "the desired state file was not read");
            }
        }
        watch.changed().await;
    }
}

/// Probes what is due, puts the result in the kernel and the routes, and writes down what
/// changed. Raced rather than slept, because the length is chosen from the state as it stands
/// now: a microVM that comes up during it is one this decision could not have known about.
pub async fn status_loop(host: Arc<Host>) {
    let versions = host_versions(&host);
    let reported_state_file = crate::report::writer::reported_state_file(&host);
    loop {
        crate::reconcile::refresh(&host).await;
        let report = crate::report::writer::build(&host, versions.clone()).await;
        crate::report::writer::write(&reported_state_file, &report);

        let now = crate::clock::now_ms();
        // Exactly the condition the fast probe grid runs on, rather than the states it tends to
        // appear in: a tick taken for something no longer being probed that fast is one taken for
        // as long as that instance is up.
        let settling = host.state.records().await.iter().any(|record| {
            crate::health::is_on_startup_grid(&record.health, &record.grace_inputs(now))
        });
        let tick = if settling { SETTLING_TICK } else { STATUS_TICK };
        tokio::select! {
            _ = tokio::time::sleep(tick) => {}
            _ = async {
                tokio::time::sleep(MIN_REFRESH_GAP).await;
                host.state.refresh_signalled().await;
            } => {}
        }
    }
}

/// Measures what the host's counters say, decides which apps have gone quiet, and lets them
/// sleep. Deciding straight after measuring, because the measurement is the only thing that moves
/// the answer: an app is let go on the reading that found it quiet rather than on a tick that
/// happened to come later.
pub async fn measurement_loop(host: Arc<Host>) {
    loop {
        tokio::time::sleep(MEASUREMENT_INTERVAL).await;
        crate::reconcile::idle::record_activity(&host).await;
        crate::reconcile::idle::apply_sleep(&host).await;
    }
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
    use super::*;
    use crate::test_support::*;

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

        let converging = tokio::spawn(converge_loop(host.arc().clone()));
        for _ in 0..200 {
            if host.state.record(&app_id()).await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        converging.abort();

        assert!(host.state.record(&app_id()).await.is_some());
        assert_eq!(host.vms.calls(), vec![crate::services::VmCall::Boot]);
        // And it is cached, so a restart during an outage of whatever writes the file converges
        // on the last thing it was given rather than on nothing.
        assert_eq!(host.cached_desired_state().await.as_ref(), Some(&desired));
    }

    /// A host with nothing on it is not a host that failed: a fresh machine has no document yet.
    #[tokio::test]
    async fn a_missing_document_is_the_ordinary_state_of_a_fresh_host() {
        let host = test_host().await;
        let converging = tokio::spawn(converge_loop(host.arc().clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        converging.abort();
        assert!(host.state.records().await.is_empty());
    }
}
