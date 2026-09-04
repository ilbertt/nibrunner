//! Everything that happens to one microVM, and the order it happens in.
//!
//! The daemon never becomes a guest's supervisor in the sense that matters: it stages the files,
//! starts a process in a session of its own, and stops caring. A daemon that goes away leaves
//! every tenant running, and the one that comes back adopts them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use protocol::AppId;

use crate::json_store::{make_directory, write_json};
use crate::logs::receiver::{tenant_log_socket_path, TenantLogReceiver};
use crate::net::tap::{HostNetwork, Neighbour, TapInterface};
use crate::services::{
    BootRequest, LogSink, SuspendRequest, VmError, Vmm,
};
use crate::state::SharedState;
use crate::vm::firecracker_api::FirecrackerApi;
use crate::vm::process::{VmProcesses, FIRECRACKER_VERSION};
use crate::vm::snapshot::{
    ensure_loadable, measure_snapshot_disk, refusal_for_disk, refusal_to_sleep, snapshot_bytes_for,
    snapshot_paths, SleepSubject, SnapshotStamp,
};
use crate::vm::status::VmStatus;
use crate::volumes::VolumeBackend;
use guest_contract::firecracker::{render_firecracker_config, VmNetwork, VmPaths, VmVsock};
use guest_contract::instance_env::{render_instance_env, InstanceEnvContent, PublicAddress};

pub const FIRECRACKER_CONFIG_FILENAME: &str = "firecracker.json";
pub const GUEST_KERNEL_FILENAME: &str = "vmlinux";
pub const GUEST_ROOTFS_FILENAME: &str = "rootfs.ext4";

const VM_DIR_MODE: u32 = 0o700;
/// Firecracker numbers guest CIDs from 3; 0, 1 and 2 are the hypervisor's own.
const FIRST_GUEST_CID: u32 = 3;

pub struct VmManager {
    pub vm_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub guest_image_dir: PathBuf,
    pub firecracker: PathBuf,
    pub guest_image_version: String,
    pub public_ipv4: Option<protocol::Ipv4Address>,
    pub processes: VmProcesses,
    pub network: Arc<dyn HostNetwork>,
    pub volumes: Arc<dyn VolumeBackend>,
    pub logs: Arc<TenantLogReceiver>,
    pub sink: Arc<dyn LogSink>,
    pub state: SharedState,
}

impl VmManager {
    pub fn working_dir_for(&self, app_id: &AppId) -> PathBuf {
        self.vm_dir.join(app_id.as_str())
    }

    fn api(&self, app_id: &AppId) -> FirecrackerApi {
        FirecrackerApi::at(self.processes.api_socket(app_id))
    }

    /// The stamp a snapshot taken now would carry, and the one a stored snapshot has to match to
    /// be loadable. Both readings are of the host as it is at this moment rather than as it was
    /// when the daemon started, because a deploy moves the guest image under a running daemon.
    fn current_stamp(&self, request: &SuspendRequest) -> SnapshotStamp {
        SnapshotStamp {
            deployment_id: request.deployment_id.clone(),
            guest_image_version: self.guest_image_version.clone(),
            host_boot_id: self.processes.boot_id().to_string(),
            slot: request.slot.slot,
        }
    }

    /// The stamp first and on its own: while it is there a start takes the snapshot beside it as
    /// an instruction, and once it is gone none of what remains is loadable by anything.
    fn discard_snapshot(&self, app_id: &AppId) {
        let paths = snapshot_paths(&self.snapshot_dir, app_id);
        let _ = std::fs::remove_file(&paths.stamp_path);
        let _ = std::fs::remove_dir_all(&paths.directory);
    }

    /// Everything a cold boot puts on disk and in the kernel before the VMM is asked for
    /// anything: the tap, the host's view of the guest behind it, the config drive and the
    /// machine description Firecracker reads.
    async fn stage(&self, request: &BootRequest) -> Result<PathBuf, VmError> {
        let slot = &request.slot;
        let host = |error: crate::net::tap::NetworkError| VmError::Host(error.message());
        self.network
            .ensure_tap(&TapInterface {
                tap_name: slot.tap_name.clone(),
                host_ipv4: slot.host_ipv4.clone(),
                subnet_prefix_length: slot.subnet_prefix_length,
            })
            .await
            .map_err(host)?;
        // Written before the guest exists rather than after it answers, because nothing has to
        // ask the guest for it: the MAC is the slot's, and the config below is what hands it
        // over. Left to ARP, the host's first probe of a new guest pays the resolution a wake
        // already knows not to spend.
        self.network
            .refresh_neighbour(&Neighbour {
                guest_ipv4: slot.guest_ipv4.clone(),
                guest_mac: slot.guest_mac.clone(),
                tap_name: slot.tap_name.clone(),
            })
            .await
            .map_err(host)?;

        let working_dir = self.working_dir_for(&request.desired.app_id);
        make_directory(&working_dir, VM_DIR_MODE).map_err(|error| VmError::Host(error.to_string()))?;

        let public_address = request
            .desired
            .config
            .has_extra_public_port
            .then(|| {
                self.public_ipv4.clone().map(|ipv4| PublicAddress { ipv4, port: slot.extra_public_port })
            })
            .flatten();
        let rendered = render_instance_env(&InstanceEnvContent {
            http_port: request.desired.config.http_port,
            public_address,
            hostnames: &request.desired.hostnames,
            args: &request.desired.config.args,
            environment: &request.desired.config.environment,
            restart_policy: &request.desired.config.restart_policy,
        })
        .map_err(|error| VmError::Host(error.to_string()))?;
        let config_image = crate::vm::artifacts::build_instance_config_image(&working_dir, &rendered)
            .map_err(|error| VmError::Host(error.message()))?;

        let config = render_firecracker_config(
            request.desired.config.resources,
            &VmPaths {
                kernel_path: self.guest_image_dir.join(GUEST_KERNEL_FILENAME).display().to_string(),
                rootfs_path: self.guest_image_dir.join(GUEST_ROOTFS_FILENAME).display().to_string(),
                artifact_image_path: request.artifact_image_path.display().to_string(),
                instance_config_image_path: config_image.display().to_string(),
                data_device_path: request.data_device_path.clone(),
            },
            &VmNetwork {
                tap_name: slot.tap_name.clone(),
                guest_mac: slot.guest_mac.clone(),
                guest_ipv4: slot.guest_ipv4.clone(),
                host_ipv4: slot.host_ipv4.clone(),
                subnet_prefix_length: slot.subnet_prefix_length,
            },
            // Relative, because Firecracker resolves it against its working directory — which is
            // this app's, so two guests never share a vsock path.
            &VmVsock {
                guest_cid: FIRST_GUEST_CID + slot.slot,
                path: guest_contract::vsock::GUEST_VSOCK_FILENAME.to_string(),
            },
        );
        let config_file = working_dir.join(FIRECRACKER_CONFIG_FILENAME);
        write_json(&config_file, &config).map_err(|error| VmError::Host(error.message()))?;

        self.logs
            .attach(
                request.desired.app_id.clone(),
                request.desired.deployment_id.clone(),
                tenant_log_socket_path(&working_dir),
                self.sink.clone(),
            )
            .await
            .map_err(|error| VmError::Host(error.to_string()))?;
        Ok(config_file)
    }

    /// Every reason this microVM must not be snapshotted now, or `None`.
    ///
    /// The states a snapshot must never be taken in are read from this daemon's own record rather
    /// than accepted from the caller, because they are the preconditions of the operation and not
    /// an opinion about it: a caller that could supply them could also forget to.
    ///
    /// A disk that cannot be measured refuses too. Sleeping is an optimisation and refusing it
    /// costs an app nothing it notices, so every doubt here resolves the same way.
    async fn refusal_to_snapshot(&self, app_id: &AppId) -> Option<String> {
        let Some(record) = self.state.record(app_id).await else {
            return refusal_to_sleep(None).map(str::to_string);
        };
        if let Some(refusal) = refusal_to_sleep(Some(SleepSubject {
            stop_requested: record.stop_requested,
            desired_running: record.desired_running,
            ever_healthy: record.health.ever_healthy,
        })) {
            return Some(refusal.to_string());
        }
        match measure_snapshot_disk(&self.snapshot_dir, 0) {
            Err(error) => {
                tracing::warn!(%app_id, %error, "snapshot disk could not be measured");
                Some("the disk it would be written to cannot be measured".into())
            }
            Ok(disk) => {
                // The one place these numbers exist, so a sleep says it on the way past whether
                // or not it is allowed to proceed.
                tracing::info!(
                    %app_id,
                    total_bytes = disk.total_bytes,
                    available_bytes = disk.available_bytes,
                    snapshot_bytes = disk.snapshot_bytes,
                    "snapshot disk measured"
                );
                refusal_for_disk(&disk, snapshot_bytes_for(record.resources.memory_mib))
            }
        }
    }
}

#[async_trait]
impl Vmm for VmManager {
    async fn boot(&self, request: BootRequest) -> Result<(), VmError> {
        let app_id = request.desired.app_id.clone();
        // Ahead of everything, because everything below replaces what a snapshot of this app was
        // taken against — and a start that still found a stamp would restore the old guest onto
        // the new deployment's disk rather than boot the new one.
        self.discard_snapshot(&app_id);
        let staged = std::time::Instant::now();
        let config_file = self.stage(&request).await?;
        let staged_ms = staged.elapsed().as_millis();

        let starting = std::time::Instant::now();
        let working_dir = self.working_dir_for(&app_id);
        let started = self
            .processes
            .spawn(&app_id, &self.firecracker, &working_dir, Some(&config_file))
            .await;
        if let Err(error) = started {
            let _ = self.logs.detach(&app_id).await;
            return Err(VmError::Host(error.to_string()));
        }
        tracing::info!(
            %app_id,
            slot = request.slot.slot,
            staged_ms,
            vmm_ms = starting.elapsed().as_millis(),
            "instance booting"
        );
        Ok(())
    }

    /// A microVM taken down at a point it can be put back on, rather than one taken down.
    ///
    /// The flush comes before the pause on purpose: one that hangs then leaves a microVM that is
    /// still serving, where a pause first would freeze the tenant for the whole of it.
    ///
    /// The stamp is written last, once the microVM is down and the files beside it are complete.
    /// Nothing before that point is loadable, which is what makes every way this can fail leave a
    /// cold boot rather than a half-restore.
    async fn sleep(&self, request: SuspendRequest) -> Result<(), VmError> {
        if let Some(reason) = self.refusal_to_snapshot(&request.app_id).await {
            return Err(VmError::SleepRefused { reason });
        }
        let paths = snapshot_paths(&self.snapshot_dir, &request.app_id);
        let stamp = self.current_stamp(&request);
        let api = self.api(&request.app_id);

        self.discard_snapshot(&request.app_id);
        make_directory(&paths.directory, VM_DIR_MODE).map_err(|error| VmError::Host(error.to_string()))?;
        if let Err(error) = self.volumes.flush().await {
            // Best-effort, and deliberately: a microVM nothing could take down because a flush
            // failed is worse than one taken down having lost what the flush would have saved.
            tracing::warn!(app_id = %request.app_id, error = %error.message(), "the volume backend would not flush");
        }

        // Timed as one window because it is one: the guest is stopped from the pause to the stop,
        // so this is what sleeping costs a tenant rather than what it costs the host.
        let paused = std::time::Instant::now();
        api.pause().await?;
        if let Err(error) = api.create_snapshot(&paths.state_path, &paths.memory_path).await {
            // A microVM left paused answers nothing and is never asked to run again.
            let _ = api.resume().await;
            return Err(error);
        }
        self.processes.stop(&request.app_id).await;

        write_json(&paths.stamp_path, &stamp).map_err(|error| VmError::Host(error.message()))?;
        let memory_bytes = std::fs::metadata(&paths.memory_path).map(|info| info.len()).unwrap_or(0);
        tracing::info!(
            app_id = %request.app_id,
            slot = request.slot.slot,
            snapshot_ms = paused.elapsed().as_millis(),
            memory_bytes,
            "instance asleep"
        );
        Ok(())
    }

    /// The microVM that went to sleep, back where it was.
    ///
    /// The stamp is checked before anything starts, and a snapshot that fails the check is thrown
    /// away rather than left: it is unloadable from here on, and a stamp on disk is what a start
    /// reads to decide it is a restore.
    async fn wake(&self, request: SuspendRequest) -> Result<(), VmError> {
        let paths = snapshot_paths(&self.snapshot_dir, &request.app_id);
        let expected = self.current_stamp(&request);
        if let Err(error) = ensure_loadable(&paths.stamp_path, &expected) {
            self.discard_snapshot(&request.app_id);
            return Err(error);
        }

        // Taking the stamp is what makes a restore happen at most once. A daemon that dies
        // between here and the load leaves a microVM that cold-boots off its disk, rather than
        // one that resumes into a guest whose disk has since moved on without it.
        let _ = std::fs::remove_file(&paths.stamp_path);

        let restoring = std::time::Instant::now();
        let working_dir = self.working_dir_for(&request.app_id);
        // No config file: `PUT /snapshot/load` is refused by a Firecracker that has been given
        // any resource but a logger.
        let started = self
            .processes
            .spawn(&request.app_id, &self.firecracker, &working_dir, None)
            .await
            .map_err(|error| VmError::Host(error.to_string()));
        let outcome = match started {
            Err(error) => Err(error),
            Ok(_) => {
                let api = self.api(&request.app_id);
                match api.load_snapshot(&paths.state_path, &paths.memory_path).await {
                    Err(error) => Err(error),
                    Ok(()) => match api.resume().await {
                        Err(error) => Err(error),
                        Ok(()) => self
                            .network
                            .refresh_neighbour(&Neighbour {
                                guest_ipv4: request.slot.guest_ipv4.clone(),
                                guest_mac: request.slot.guest_mac.clone(),
                                tap_name: request.slot.tap_name.clone(),
                            })
                            .await
                            .map_err(|error| VmError::Host(error.message())),
                    },
                }
            }
        };
        if outcome.is_err() {
            // The start already consumed the stamp, so this Firecracker holds no guest and never
            // will: stopping it is what leaves a cold boot rather than a process in the way of one.
            self.processes.stop(&request.app_id).await;
        }
        // Every way out, so no retry of a restore that failed halfway can find the files it did
        // not finish with. The stamp is already gone and would stop a second load on its own;
        // this is what keeps at-most-once from resting on that single fact.
        self.discard_snapshot(&request.app_id);
        outcome?;
        tracing::info!(
            app_id = %request.app_id,
            slot = request.slot.slot,
            restore_ms = restoring.elapsed().as_millis(),
            "instance awake"
        );
        Ok(())
    }

    /// Before the stop, so a stop that fails leaves a running microVM and never a stale snapshot.
    async fn stop(&self, app_id: &AppId) -> Result<(), VmError> {
        self.discard_snapshot(app_id);
        self.processes.stop(app_id).await;
        Ok(())
    }

    async fn discard(&self, app_id: &AppId) -> Result<(), VmError> {
        self.discard_snapshot(app_id);
        self.processes.forget(app_id);
        self.logs.detach(app_id).await;
        let _ = std::fs::remove_dir_all(self.working_dir_for(app_id));
        Ok(())
    }

    async fn statuses(&self, app_ids: &[AppId]) -> std::collections::BTreeMap<AppId, VmStatus> {
        app_ids.iter().map(|app_id| (app_id.clone(), self.processes.status(app_id))).collect()
    }

    async fn adopted_app_ids(&self) -> Vec<AppId> {
        self.processes.adopted_app_ids()
    }

    /// `/init` ends every way it can stop with a line saying which one it took, so the last of
    /// them is its verdict. The console is truncated on every start, so the whole file belongs to
    /// the run in progress and nothing has to bound the read by time.
    async fn guest_verdict(&self, app_id: &AppId) -> Option<String> {
        let console = std::fs::read_to_string(self.processes.console_path(app_id)).ok()?;
        guest_contract::control::last_guest_line(&console)
    }

    fn working_dir(&self, app_id: &AppId) -> PathBuf {
        self.working_dir_for(app_id)
    }
}

/// The version the guest image names itself, read out of the manifest the build wrote beside it.
/// A host with no manifest reports the Firecracker it carries and `unknown` for the image, which
/// is enough to invalidate a snapshot but not enough to claim a version it cannot see.
pub fn read_guest_image_version(guest_image_dir: &Path) -> String {
    let manifest: Option<serde_json::Value> =
        crate::json_store::read_json(&guest_image_dir.join("manifest.json")).ok().flatten();
    manifest
        .and_then(|manifest| manifest.get("version").and_then(|value| value.as_str()).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn firecracker_version() -> &'static str {
    FIRECRACKER_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::FileLogSink;
    use crate::net::tap::RecordingNetwork;
    use crate::services::RecordingCommandRunner;
    use crate::state::HostState;
    use crate::test_support::*;
    use crate::volumes::local_file::LocalFileVolumes;
    use protocol::ObjectKey;

    struct Fixture {
        _directory: tempfile::TempDir,
        manager: VmManager,
        network: Arc<RecordingNetwork>,
        state: SharedState,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let network = RecordingNetwork::new();
        let state = HostState::shared();
        let manager = VmManager {
            vm_dir: root.join("vm"),
            snapshot_dir: root.join("snapshots"),
            guest_image_dir: root.join("guest"),
            firecracker: root.join("bin/firecracker"),
            guest_image_version: "6.1.180-test".into(),
            public_ipv4: None,
            processes: VmProcesses::new(root.join("run")),
            network: network.clone(),
            volumes: Arc::new(LocalFileVolumes::new(
                root.join("volumes"),
                ObjectKey::parse("volumes").unwrap(),
                RecordingCommandRunner::succeeding(),
            )),
            logs: TenantLogReceiver::new(),
            sink: Arc::new(FileLogSink::new(root.join("logs"))),
            state: state.clone(),
        };
        Fixture { _directory: directory, manager, network, state }
    }

    /// What the guest's init reads off `vdc`, read back out of the image the way it would.
    fn config_drive(working_dir: &Path) -> String {
        use std::io::Read;
        let image = std::fs::read(working_dir.join("config.squashfs")).unwrap();
        let filesystem = backhand::FilesystemReader::from_reader(std::io::Cursor::new(image)).unwrap();
        let node = filesystem
            .files()
            .find(|node| node.fullpath.to_string_lossy() == "/instance.env")
            .expect("the config drive holds instance.env");
        let backhand::InnerNode::File(file) = &node.inner else {
            panic!("instance.env is not a file");
        };
        let mut bytes = Vec::new();
        filesystem.file(file).reader().read_to_end(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn boot_request(desired: protocol::DesiredInstance) -> BootRequest {
        BootRequest {
            slot: nft_render::describe_slot(0, desired.app_id.clone()),
            desired,
            data_device_path: "/dev/loop0".into(),
            artifact_image_path: PathBuf::from("/cache/abc/artifact.squashfs"),
        }
    }

    /// Everything a cold boot puts on the host before the VMM is asked for anything, asserted as
    /// what it wrote rather than as calls it made.
    #[tokio::test]
    async fn staging_writes_the_machine_description_the_boot_contract_names() {
        let fixture = fixture();
        let request = boot_request(desired_instance(|instance| instance.hostnames = vec![app_hostname()]));
        let config_file = fixture.manager.stage(&request).await.unwrap();

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_file).unwrap()).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 4);
        assert!(drives[0]["path_on_host"].as_str().unwrap().ends_with("guest/rootfs.ext4"));
        assert!(drives[1]["path_on_host"].as_str().unwrap().ends_with("artifact.squashfs"));
        assert!(drives[2]["path_on_host"].as_str().unwrap().ends_with("config.squashfs"));
        assert_eq!(drives[3]["path_on_host"], "/dev/loop0");
        assert_eq!(drives[3]["cache_type"], "Writeback");
        assert!(config["boot-source"]["boot_args"].as_str().unwrap().contains("clocksource=kvm-clock"));
        // Relative, so Firecracker resolves it inside this app's own directory.
        assert_eq!(config["vsock"]["uds_path"], "logs.vsock");
        assert_eq!(config["vsock"]["guest_cid"], 3);

        // The tap and the neighbour are both written before the guest exists.
        assert_eq!(fixture.network.taps()[0].tap_name, "nbr0");
        assert_eq!(fixture.network.neighbours()[0].guest_mac, "02:00:0a:c9:00:02");
        // And the host is listening for the guest's own output before it can write any.
        assert_eq!(fixture.manager.logs.attached().await, vec![app_id()]);
    }

    /// The runtime refuses a key it does not know and reads both public-port lines as optional,
    /// so an app that asked for no port has to be a file with neither line rather than one with
    /// an empty value.
    #[tokio::test]
    async fn an_app_that_asked_for_no_public_port_is_sent_neither_half_of_one() {
        let fixture = fixture();
        fixture.manager.stage(&boot_request(desired_instance(|_| {}))).await.unwrap();
        let written = config_drive(&fixture.manager.working_dir_for(&app_id()));
        assert!(!written.contains("NIBRUN_PUBLIC_IPV4"));
        assert!(!written.contains("NIBRUN_EXTRA_PUBLIC_PORT"));
        assert!(written.contains("NIBRUN_HTTP_PORT=3000"));
    }

    /// The two halves travel together or not at all, and the address is the relay's rather than
    /// this host's — which is why a guest cannot work it out for itself.
    #[tokio::test]
    async fn an_app_that_asked_is_told_the_address_and_the_port_together() {
        let mut fixture = fixture();
        fixture.manager.public_ipv4 = Some(protocol::Ipv4Address::parse("203.0.113.7").unwrap());
        let wants_a_port = desired_instance(|instance| instance.config.has_extra_public_port = true);
        fixture.manager.stage(&boot_request(wants_a_port)).await.unwrap();
        let written = config_drive(&fixture.manager.working_dir_for(&app_id()));
        assert!(written.contains("NIBRUN_PUBLIC_IPV4=203.0.113.7"));
        assert!(written.contains("NIBRUN_EXTRA_PUBLIC_PORT=22000"));
    }

    /// A guest may only be captured where waking it is survivable, and the record is what says so.
    #[tokio::test]
    async fn a_sleep_is_refused_for_every_reason_the_record_gives() {
        let fixture = fixture();
        let request = SuspendRequest {
            app_id: app_id(),
            deployment_id: deployment_id(),
            slot: nft_render::describe_slot(0, app_id()),
        };
        // A host that holds no record of it.
        let refused = fixture.manager.sleep(request.clone()).await.unwrap_err();
        assert!(refused.message().contains("holds no record"));

        fixture.state.put_record(instance_record(|record| record.health.ever_healthy = false)).await;
        let never_answered = fixture.manager.sleep(request.clone()).await.unwrap_err();
        assert!(never_answered.message().contains("finished booting"));

        fixture
            .state
            .put_record(instance_record(|record| {
                record.health.ever_healthy = true;
                record.stop_requested = true;
            }))
            .await;
        let stopping = fixture.manager.sleep(request).await.unwrap_err();
        assert!(stopping.message().contains("asked to stop"));
    }

    /// A snapshot nothing can load is thrown away rather than left for a later start to find and
    /// take as an instruction.
    #[tokio::test]
    async fn a_wake_with_no_loadable_snapshot_says_so_and_leaves_nothing_behind() {
        let fixture = fixture();
        let request = SuspendRequest {
            app_id: app_id(),
            deployment_id: deployment_id(),
            slot: nft_render::describe_slot(0, app_id()),
        };
        let paths = snapshot_paths(&fixture.manager.snapshot_dir, &app_id());
        std::fs::create_dir_all(&paths.directory).unwrap();
        std::fs::write(&paths.memory_path, b"pretend memory").unwrap();
        write_json(
            &paths.stamp_path,
            &SnapshotStamp {
                deployment_id: deployment_id(),
                guest_image_version: "an-older-image".into(),
                host_boot_id: fixture.manager.processes.boot_id().to_string(),
                slot: 0,
            },
        )
        .unwrap();

        let error = fixture.manager.wake(request).await.unwrap_err();
        assert!(matches!(error, VmError::SnapshotUnusable { .. }));
        assert!(error.message().contains("guest image has changed"));
        assert!(!paths.directory.exists(), "an unloadable snapshot is discarded, not left");
    }

    #[tokio::test]
    async fn discarding_takes_the_working_directory_the_record_and_the_log_attachment() {
        let fixture = fixture();
        fixture.manager.stage(&boot_request(desired_instance(|_| {}))).await.unwrap();
        assert!(fixture.manager.working_dir_for(&app_id()).exists());
        fixture.manager.discard(&app_id()).await.unwrap();
        assert!(!fixture.manager.working_dir_for(&app_id()).exists());
        assert!(fixture.manager.logs.attached().await.is_empty());
        assert_eq!(fixture.manager.statuses(&[app_id()]).await[&app_id()], VmStatus::default());
    }

    #[tokio::test]
    async fn the_guest_verdict_is_the_last_thing_its_init_said() {
        let fixture = fixture();
        make_directory(fixture.manager.processes.console_path(&app_id()).parent().unwrap(), 0o700).unwrap();
        std::fs::write(
            fixture.manager.processes.console_path(&app_id()),
            "[nibrun] starting the tenant\n[nibrun] the tenant has stopped; shutting the guest down\n[   15.7] reboot: Restarting system\n",
        )
        .unwrap();
        assert_eq!(
            fixture.manager.guest_verdict(&app_id()).await.as_deref(),
            Some("the tenant has stopped; shutting the guest down")
        );
    }

    #[test]
    fn the_guest_image_names_its_own_version_and_an_absent_one_is_not_invented() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(read_guest_image_version(directory.path()), "unknown");
        std::fs::write(directory.path().join("manifest.json"), r#"{"version":"6.1.180-98db6df338f0"}"#).unwrap();
        assert_eq!(read_guest_image_version(directory.path()), "6.1.180-98db6df338f0");
    }
}
