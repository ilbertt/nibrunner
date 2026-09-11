use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use protocol::AppId;

use crate::adapters::logs::receiver::{tenant_log_socket_path, TenantLogReceiver};
use crate::adapters::net::tap::{HostNetwork, Neighbour, TapInterface};
use crate::adapters::vm::firecracker_api::FirecrackerApi;
use crate::adapters::vm::process::{VmProcesses, FIRECRACKER_VERSION};
use crate::adapters::vm::snapshot::{
    ensure_loadable, measure_snapshot_disk, refusal_for_disk, refusal_to_sleep, snapshot_bytes_for,
    snapshot_paths, SleepSubject, SnapshotStamp,
};
use crate::adapters::vm::status::VmStatus;
use crate::adapters::volumes::VolumeBackend;
use crate::json_store::{make_directory, write_json};
use crate::ports::{BootRequest, LogSink, SuspendRequest, VmError, Vmm};
use crate::state::SharedState;
use guest_contract::firecracker::{render_firecracker_config, VmNetwork, VmPaths, VmVsock};
use guest_contract::instance_env::{render_instance_env, InstanceEnvContent};

pub const FIRECRACKER_CONFIG_FILENAME: &str = "firecracker.json";
pub const GUEST_KERNEL_FILENAME: &str = "vmlinux";
pub const GUEST_ROOTFS_FILENAME: &str = "rootfs.ext4";
pub const GUEST_MANIFEST_FILENAME: &str = "manifest.json";

const VM_DIR_MODE: u32 = 0o700;
const FIRST_GUEST_CID: u32 = 3;

pub struct VmManager {
    pub vm_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub guest_image_dir: PathBuf,
    pub firecracker: PathBuf,
    pub guest_image_version: String,
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

    fn current_stamp(&self, request: &SuspendRequest) -> SnapshotStamp {
        SnapshotStamp {
            deployment_id: request.deployment_id.clone(),
            guest_image_version: self.guest_image_version.clone(),
            host_boot_id: self.processes.boot_id().to_string(),
            slot: request.slot.slot,
        }
    }

    fn discard_snapshot(&self, app_id: &AppId) {
        let paths = snapshot_paths(&self.snapshot_dir, app_id);
        let _ = std::fs::remove_file(&paths.stamp_path);
        let _ = std::fs::remove_dir_all(&paths.directory);
    }

    async fn stage(&self, request: &BootRequest) -> Result<PathBuf, VmError> {
        let slot = &request.slot;
        let host = |error: crate::adapters::net::tap::NetworkError| VmError::Host(error.message());
        self.network
            .ensure_tap(&TapInterface {
                tap_name: slot.tap_name.clone(),
                host_ipv4: slot.host_ipv4.clone(),
                subnet_prefix_length: slot.subnet_prefix_length,
            })
            .await
            .map_err(host)?;
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

        let rendered = render_instance_env(&InstanceEnvContent {
            http_port: request.desired.config.http_port,
            layers: request.payload.layer_image_paths.len(),
            hostnames: &request.desired.hostnames,
            args: &request.desired.config.args,
            environment: &request.desired.config.environment,
            restart_policy: &request.desired.config.restart_policy,
        })
        .map_err(|error| VmError::Host(error.to_string()))?;
        let config_image = crate::adapters::vm::layers::build_instance_config_image(&working_dir, &rendered)
            .map_err(|error| VmError::Host(error.message()))?;

        let config = render_firecracker_config(
            request.desired.config.resources,
            &VmPaths {
                kernel_path: self
                    .guest_image_dir
                    .join(GUEST_KERNEL_FILENAME)
                    .display()
                    .to_string(),
                rootfs_path: self
                    .guest_image_dir
                    .join(GUEST_ROOTFS_FILENAME)
                    .display()
                    .to_string(),
                instance_config_image_path: config_image.display().to_string(),
                data_device_path: request.data_device_path.clone(),
                layer_image_paths: request
                    .payload
                    .layer_image_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect(),
            },
            &VmNetwork {
                tap_name: slot.tap_name.clone(),
                guest_mac: slot.guest_mac.clone(),
                guest_ipv4: slot.guest_ipv4.clone(),
                host_ipv4: slot.host_ipv4.clone(),
                subnet_prefix_length: slot.subnet_prefix_length,
            },
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
        match measure_snapshot_disk(&self.snapshot_dir, self.volumes.reserved_cache().disk_bytes) {
            Err(error) => {
                tracing::warn!(%app_id, %error, "snapshot disk could not be measured");
                Some("the disk it would be written to cannot be measured".into())
            }
            Ok(disk) => {
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
            tracing::warn!(app_id = %request.app_id, error = %error.message(), "the volume backend would not flush");
        }

        let paused = std::time::Instant::now();
        api.pause().await?;
        if let Err(error) = api.create_snapshot(&paths.state_path, &paths.memory_path).await {
            let _ = api.resume().await;
            return Err(error);
        }
        self.processes.stop(&request.app_id).await;

        write_json(&paths.stamp_path, &stamp).map_err(|error| VmError::Host(error.message()))?;
        let memory_bytes = std::fs::metadata(&paths.memory_path)
            .map(|info| info.len())
            .unwrap_or(0);
        tracing::info!(
            app_id = %request.app_id,
            slot = request.slot.slot,
            snapshot_ms = paused.elapsed().as_millis(),
            memory_bytes,
            "instance asleep"
        );
        Ok(())
    }

    async fn wake(&self, request: SuspendRequest) -> Result<(), VmError> {
        let paths = snapshot_paths(&self.snapshot_dir, &request.app_id);
        let expected = self.current_stamp(&request);
        if let Err(error) = ensure_loadable(&paths.stamp_path, &expected) {
            self.discard_snapshot(&request.app_id);
            return Err(error);
        }

        let _ = std::fs::remove_file(&paths.stamp_path);

        let restoring = std::time::Instant::now();
        let working_dir = self.working_dir_for(&request.app_id);
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
            self.processes.stop(&request.app_id).await;
        }
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

    async fn delete_tap(&self, tap_name: &str) -> Result<(), VmError> {
        self.network
            .delete_tap(tap_name)
            .await
            .map_err(|error| VmError::Host(error.message()))
    }

    async fn tap_names(&self) -> Vec<String> {
        self.network.tap_names().await
    }

    async fn statuses(&self, app_ids: &[AppId]) -> std::collections::BTreeMap<AppId, VmStatus> {
        app_ids
            .iter()
            .map(|app_id| (app_id.clone(), self.processes.status(app_id)))
            .collect()
    }

    async fn adopted_app_ids(&self) -> Vec<AppId> {
        self.processes.adopted_app_ids()
    }

    async fn guest_verdict(&self, app_id: &AppId) -> Option<String> {
        let console = std::fs::read_to_string(self.processes.console_path(app_id)).ok()?;
        guest_contract::control::last_guest_line(&console)
    }

    fn working_dir(&self, app_id: &AppId) -> PathBuf {
        self.working_dir_for(app_id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GuestImageError {
    #[error("{path} is not there, so this host has no guest image to boot a tenant from")]
    Missing { path: String },
    #[error("{name} in {directory} is {actual}, not the {expected} its manifest describes")]
    Altered {
        name: String,
        directory: String,
        expected: String,
        actual: String,
    },
    #[error("the guest image manifest in {directory} could not be read: {reason}")]
    Unreadable { directory: String, reason: String },
}

impl GuestImageError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

pub fn verify_guest_image(guest_image_dir: &Path) -> Result<String, GuestImageError> {
    let unreadable = |reason: &str| GuestImageError::Unreadable {
        directory: guest_image_dir.display().to_string(),
        reason: reason.to_string(),
    };
    let manifest_path = guest_image_dir.join(GUEST_MANIFEST_FILENAME);
    let manifest: serde_json::Value = crate::json_store::read_json(&manifest_path)
        .map_err(|error| unreadable(&error.message()))?
        .ok_or_else(|| GuestImageError::Missing {
            path: manifest_path.display().to_string(),
        })?;
    let version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| unreadable("it names no version"))?
        .to_string();
    let described = manifest
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| unreadable("it describes no artifacts"))?;

    for name in [GUEST_KERNEL_FILENAME, GUEST_ROOTFS_FILENAME] {
        let expected = described
            .iter()
            .find(|artifact| artifact.get("name").and_then(serde_json::Value::as_str) == Some(name))
            .and_then(|artifact| artifact.get("sha256"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| unreadable(&format!("it describes no {name}")))?;
        let path = guest_image_dir.join(name);
        let bytes = std::fs::read(&path).map_err(|_| GuestImageError::Missing {
            path: path.display().to_string(),
        })?;
        let actual = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        if actual != expected {
            return Err(GuestImageError::Altered {
                name: name.to_string(),
                directory: guest_image_dir.display().to_string(),
                expected: expected.to_string(),
                actual,
            });
        }
    }
    Ok(version)
}

pub fn firecracker_version() -> &'static str {
    FIRECRACKER_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::logs::FileLogSink;
    use crate::adapters::volumes::local_file::LocalFileVolumes;
    use crate::state::HostState;
    use crate::test_support::mocks;
    use crate::test_support::*;
    use protocol::ObjectKey;

    struct Fixture {
        _directory: tempfile::TempDir,
        manager: VmManager,
        network: mocks::NetworkSpy,
        state: SharedState,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (network, network_spy) = mocks::network();
        let state = HostState::shared();
        let manager = VmManager {
            vm_dir: root.join("vm"),
            snapshot_dir: root.join("snapshots"),
            guest_image_dir: root.join("guest"),
            firecracker: root.join("bin/firecracker"),
            guest_image_version: "6.1.180-test".into(),
            processes: VmProcesses::new(root.join("run")),
            network,
            volumes: Arc::new(LocalFileVolumes::new(
                root.join("volumes"),
                ObjectKey::parse("volumes").unwrap(),
                mocks::commands_succeeding().0,
            )),
            logs: TenantLogReceiver::new(),
            sink: Arc::new(FileLogSink::new(root.join("logs"))),
            state: state.clone(),
        };
        Fixture {
            _directory: directory,
            manager,
            network: network_spy,
            state,
        }
    }

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
            payload: crate::ports::PreparedPayload {
                layer_image_paths: vec![PathBuf::from("/cache/abc/packed-0123456789abcdef.squashfs")],
            },
        }
    }

    #[tokio::test]
    async fn staging_writes_the_machine_description_the_boot_contract_names() {
        let fixture = fixture();
        let request = boot_request(desired_instance(|instance| {
            instance.hostnames = vec![app_hostname()]
        }));
        let config_file = fixture.manager.stage(&request).await.unwrap();

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_file).unwrap()).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 4);
        assert!(drives[0]["path_on_host"]
            .as_str()
            .unwrap()
            .ends_with("guest/rootfs.ext4"));
        assert!(drives[1]["path_on_host"]
            .as_str()
            .unwrap()
            .ends_with("config.squashfs"));
        assert_eq!(drives[2]["path_on_host"], "/dev/loop0");
        assert_eq!(drives[2]["cache_type"], "Writeback");
        assert!(drives[3]["path_on_host"]
            .as_str()
            .unwrap()
            .ends_with("packed-0123456789abcdef.squashfs"));
        assert!(config["boot-source"]["boot_args"]
            .as_str()
            .unwrap()
            .contains("clocksource=kvm-clock"));
        assert_eq!(config["vsock"]["uds_path"], "logs.vsock");
        assert_eq!(config["vsock"]["guest_cid"], 3);

        assert_eq!(fixture.network.taps()[0].tap_name, "nbr0");
        assert_eq!(fixture.network.neighbours()[0].guest_mac, "02:00:0a:c9:00:02");
        assert_eq!(fixture.manager.logs.attached().await, vec![app_id()]);
    }

    #[tokio::test]
    async fn the_config_drive_carries_the_port_the_tenant_was_told_to_listen_on_and_how_many_layers_follow() {
        let fixture = fixture();
        let mut request = boot_request(desired_instance(|_| {}));
        request
            .payload
            .layer_image_paths
            .push(PathBuf::from("/cache/def/layer.img"));
        fixture.manager.stage(&request).await.unwrap();
        let written = config_drive(&fixture.manager.working_dir_for(&app_id()));
        assert!(written.contains("NIBRUN_HTTP_PORT=3000"));
        assert!(written.contains("NIBRUN_LAYERS=2\n"), "{written}");
    }

    #[tokio::test]
    async fn a_sleep_is_refused_for_every_reason_the_record_gives() {
        let fixture = fixture();
        let request = SuspendRequest {
            app_id: app_id(),
            deployment_id: deployment_id(),
            slot: nft_render::describe_slot(0, app_id()),
        };
        let refused = fixture.manager.sleep(request.clone()).await.unwrap_err();
        assert!(refused.message().contains("holds no record"));

        fixture
            .state
            .put_record(instance_record(|record| record.health.ever_healthy = false))
            .await;
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
        assert!(
            !paths.directory.exists(),
            "an unloadable snapshot is discarded, not left"
        );
    }

    #[tokio::test]
    async fn discarding_takes_the_working_directory_the_record_and_the_log_attachment() {
        let fixture = fixture();
        fixture
            .manager
            .stage(&boot_request(desired_instance(|_| {})))
            .await
            .unwrap();
        assert!(fixture.manager.working_dir_for(&app_id()).exists());
        fixture.manager.discard(&app_id()).await.unwrap();
        assert!(!fixture.manager.working_dir_for(&app_id()).exists());
        assert!(fixture.manager.logs.attached().await.is_empty());
        assert_eq!(
            fixture.manager.statuses(&[app_id()]).await[&app_id()],
            VmStatus::default()
        );
    }

    #[tokio::test]
    async fn the_guest_verdict_is_the_last_thing_its_init_said() {
        let fixture = fixture();
        make_directory(
            fixture
                .manager
                .processes
                .console_path(&app_id())
                .parent()
                .unwrap(),
            0o700,
        )
        .unwrap();
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

    #[tokio::test]
    async fn a_boot_a_tap_could_not_be_made_for_starts_no_hypervisor_at_all() {
        let mut fixture = fixture();
        fixture.manager.network = mocks::network_refusing(crate::adapters::net::tap::NetworkError {
            what: "a tap device",
            device: "nbr0".into(),
            reason: "operation not permitted".into(),
        });
        let error = fixture
            .manager
            .boot(boot_request(desired_instance(|_| {})))
            .await
            .unwrap_err();
        assert!(matches!(error, VmError::Host(_)), "{error}");
        assert!(error.message().contains("operation not permitted"), "{error}");
        assert!(fixture.manager.processes.read_record(&app_id()).is_none());
        assert!(fixture.manager.logs.attached().await.is_empty());
    }

    #[tokio::test]
    async fn a_hypervisor_that_would_not_start_takes_the_log_attachment_down_behind_it() {
        let fixture = fixture();
        let error = fixture
            .manager
            .boot(boot_request(desired_instance(|_| {})))
            .await
            .unwrap_err();
        assert!(matches!(error, VmError::Host(_)), "{error}");
        assert!(
            fixture.manager.logs.attached().await.is_empty(),
            "a socket nothing will write to is not left listening"
        );
    }

    #[tokio::test]
    async fn a_stop_takes_the_snapshot_with_it_so_nothing_is_woken_back_into_the_old_state() {
        let fixture = fixture();
        let paths = snapshot_paths(&fixture.manager.snapshot_dir, &app_id());
        make_directory(&paths.directory, 0o700).unwrap();
        std::fs::write(&paths.memory_path, b"pretend memory").unwrap();
        write_json(
            &paths.stamp_path,
            &SnapshotStamp {
                deployment_id: deployment_id(),
                guest_image_version: fixture.manager.guest_image_version.clone(),
                host_boot_id: fixture.manager.processes.boot_id().to_string(),
                slot: 0,
            },
        )
        .unwrap();

        fixture.manager.stop(&app_id()).await.unwrap();
        assert!(!paths.directory.exists());
        assert!(!paths.stamp_path.exists());
    }

    #[tokio::test]
    async fn a_microvm_nothing_was_ever_recorded_for_is_still_stopped_and_discarded_cleanly() {
        let fixture = fixture();
        fixture.manager.stop(&app_id()).await.unwrap();
        fixture.manager.discard(&app_id()).await.unwrap();
        assert!(fixture.manager.adopted_app_ids().await.is_empty());
        assert_eq!(fixture.manager.guest_verdict(&app_id()).await, None);
    }

    #[tokio::test]
    async fn a_console_that_holds_nothing_the_guest_said_gives_no_verdict() {
        let fixture = fixture();
        make_directory(
            fixture
                .manager
                .processes
                .console_path(&app_id())
                .parent()
                .unwrap(),
            0o700,
        )
        .unwrap();
        std::fs::write(
            fixture.manager.processes.console_path(&app_id()),
            "[    0.0] Linux version 6.1.180\n[   15.7] reboot: Restarting system\n",
        )
        .unwrap();
        assert_eq!(fixture.manager.guest_verdict(&app_id()).await, None);
    }

    #[tokio::test]
    async fn the_working_directory_a_boot_uses_is_the_one_the_rest_of_the_host_is_told_about() {
        let fixture = fixture();
        assert_eq!(
            fixture.manager.working_dir(&app_id()),
            fixture.manager.working_dir_for(&app_id())
        );
        assert!(fixture
            .manager
            .working_dir(&app_id())
            .starts_with(&fixture.manager.vm_dir));
        assert_ne!(
            fixture.manager.working_dir(&app_id()),
            fixture
                .manager
                .working_dir(&protocol::AppId::parse("app-2").unwrap())
        );
    }

    #[tokio::test]
    async fn the_status_of_every_app_asked_after_is_answered_for_even_when_none_is_running() {
        let fixture = fixture();
        let neighbour = protocol::AppId::parse("app-2").unwrap();
        let statuses = fixture.manager.statuses(&[app_id(), neighbour.clone()]).await;
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[&app_id()], VmStatus::default());
        assert_eq!(statuses[&neighbour], VmStatus::default());
        assert!(fixture.manager.statuses(&[]).await.is_empty());
    }

    #[tokio::test]
    async fn a_sleep_the_disk_cannot_hold_is_refused_before_the_microvm_is_paused() {
        let mut fixture = fixture();
        // A directory under a regular file is one nobody can make. A path that merely does not
        // exist is no refusal at all for the root a host runs this daemon as: the measure makes
        // the directory first, so /nowhere would simply be created, the disk would read fine, and
        // the sleep would carry on to a microVM that is not there.
        let elsewhere = tempfile::tempdir().unwrap();
        let occupied = elsewhere.path().join("occupied");
        std::fs::write(&occupied, b"").unwrap();
        fixture.manager.snapshot_dir = occupied.join("snapshots");
        fixture
            .state
            .put_record(instance_record(|record| record.health.ever_healthy = true))
            .await;
        let error = fixture
            .manager
            .sleep(SuspendRequest {
                app_id: app_id(),
                deployment_id: deployment_id(),
                slot: nft_render::describe_slot(0, app_id()),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, VmError::SleepRefused { .. }), "{error}");
        assert!(error.message().contains("cannot be measured"), "{error}");
    }

    #[tokio::test]
    async fn a_wake_with_no_snapshot_kept_at_all_says_so_rather_than_starting_a_cold_guest() {
        let fixture = fixture();
        let error = fixture
            .manager
            .wake(SuspendRequest {
                app_id: app_id(),
                deployment_id: deployment_id(),
                slot: nft_render::describe_slot(0, app_id()),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, VmError::SnapshotUnusable { .. }), "{error}");
        assert!(error.message().contains("kept none"), "{error}");
    }

    #[tokio::test]
    async fn staging_the_same_app_twice_rebuilds_its_config_rather_than_refusing() {
        let fixture = fixture();
        fixture
            .manager
            .stage(&boot_request(desired_instance(|_| {})))
            .await
            .unwrap();
        let changed = desired_instance(|instance| {
            instance.config.environment = tenant_environment(&[("MODE", "production")])
        });
        fixture.manager.stage(&boot_request(changed)).await.unwrap();
        let written = config_drive(&fixture.manager.working_dir_for(&app_id()));
        assert!(written.contains("MODE"), "{written}");
        assert_eq!(fixture.manager.logs.attached().await, vec![app_id()]);
    }

    #[test]
    fn this_build_names_the_hypervisor_it_carries() {
        assert_eq!(firecracker_version(), FIRECRACKER_VERSION);
        assert!(!firecracker_version().is_empty());
    }

    fn image(directory: &Path, kernel: &[u8], rootfs: &[u8]) -> String {
        std::fs::write(directory.join(GUEST_KERNEL_FILENAME), kernel).unwrap();
        std::fs::write(directory.join(GUEST_ROOTFS_FILENAME), rootfs).unwrap();
        let digest = |bytes: &[u8]| hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes));
        let manifest = serde_json::json!({
            "version": "6.1.180-nibrunner-init",
            "artifacts": [
                {"name": GUEST_KERNEL_FILENAME, "sha256": digest(kernel)},
                {"name": GUEST_ROOTFS_FILENAME, "sha256": digest(rootfs)},
            ],
        });
        std::fs::write(
            directory.join(GUEST_MANIFEST_FILENAME),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        "6.1.180-nibrunner-init".to_string()
    }

    #[test]
    fn an_image_that_is_what_its_manifest_describes_names_its_version() {
        let directory = tempfile::tempdir().unwrap();
        let version = image(directory.path(), b"a kernel", b"a root filesystem");
        assert_eq!(verify_guest_image(directory.path()).unwrap(), version);
    }

    #[test]
    fn a_host_with_no_guest_image_refuses_rather_than_failing_at_the_first_deploy() {
        let directory = tempfile::tempdir().unwrap();
        let Err(error) = verify_guest_image(directory.path()) else {
            panic!("a host with nothing to boot a tenant from was accepted");
        };
        assert!(matches!(error, GuestImageError::Missing { .. }), "{error}");
    }

    #[test]
    fn a_kernel_that_is_not_what_the_manifest_describes_is_never_booted() {
        let directory = tempfile::tempdir().unwrap();
        image(directory.path(), b"a kernel", b"a root filesystem");
        std::fs::write(directory.path().join(GUEST_KERNEL_FILENAME), b"something else").unwrap();

        let Err(error) = verify_guest_image(directory.path()) else {
            panic!("a kernel nothing vouches for was accepted");
        };
        assert!(matches!(error, GuestImageError::Altered { .. }), "{error}");
        assert!(error.message().contains(GUEST_KERNEL_FILENAME), "{error}");
    }

    #[test]
    fn a_truncated_root_filesystem_is_caught_before_a_tenant_boots_off_it() {
        let directory = tempfile::tempdir().unwrap();
        image(directory.path(), b"a kernel", b"a root filesystem");
        std::fs::write(directory.path().join(GUEST_ROOTFS_FILENAME), b"a root file").unwrap();
        assert!(verify_guest_image(directory.path()).is_err());
    }

    #[test]
    fn a_root_filesystem_the_manifest_never_mentions_is_refused_rather_than_trusted() {
        let directory = tempfile::tempdir().unwrap();
        image(directory.path(), b"a kernel", b"a root filesystem");
        let manifest = serde_json::json!({"version": "6.1.180", "artifacts": []});
        std::fs::write(
            directory.path().join(GUEST_MANIFEST_FILENAME),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        assert!(verify_guest_image(directory.path()).is_err());
    }

    #[test]
    fn a_manifest_that_names_no_version_is_not_an_image_this_host_can_report() {
        let directory = tempfile::tempdir().unwrap();
        image(directory.path(), b"a kernel", b"a root filesystem");
        for manifest in ["{}", r#"{"version":7}"#, "not json"] {
            std::fs::write(directory.path().join(GUEST_MANIFEST_FILENAME), manifest).unwrap();
            assert!(verify_guest_image(directory.path()).is_err(), "{manifest}");
        }
    }
}
