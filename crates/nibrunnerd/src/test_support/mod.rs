pub mod mocks;

use std::ops::Deref;
use std::sync::Arc;

use protocol::*;
use tokio::sync::Mutex;

use crate::domain::backoff::NO_START_ATTEMPTS;
use crate::domain::health::initial_tracker;
use crate::domain::reconcile::plan::{ObservedInstance, ObservedState, ObservedVolume};
use crate::domain::report::instance_record::{InstanceRecord, RecordFields};

pub const VOLUME_SIZE_BYTES: u64 = 4_096;
pub const OBSERVED_AT: &str = "2026-08-03T10:00:00.000Z";
pub const HOST_STORAGE_PREFIX: &str = "filesystems/host-1";
pub const ARTIFACT_BYTES: &[u8] = b"#!/usr/bin/env fake-binary\n";
pub const ARTIFACT_DIGEST: &str = "8eacc8ea7f20363ff4eeb79bc80edf5926effee2e7e13207a198ce341a0326f5";

/// A zerofs host laid out the way `nibrunnerd install` lays one out, so a test that is about one
/// field says only that field.
pub fn zerofs_settings(
    with: impl FnOnce(&mut crate::config::ZerofsSettings),
) -> crate::config::ZerofsSettings {
    let mut settings = crate::config::ZerofsSettings {
        binary: "/opt/nibrunner/bin/zerofs".into(),
        config_file: "/etc/zerofs/config.toml".into(),
        mount_path: "/mnt/zerofs".into(),
        nbd_socket_path: "/run/zerofs/nbd.sock".into(),
        ninep_socket_path: "/run/zerofs/9p.sock".into(),
        rpc_socket_path: "/run/zerofs/rpc.sock".into(),
        storage_url: "s3://filesystems/host-1".to_string(),
        cache_dir: "/data/zerofs".into(),
        cache_disk_mib: 70 * 1024,
        cache_memory_mib: 2 * 1024,
        checkpoint_runtime_dir: "/run/zerofs-checkpoint".into(),
        checkpoint_config_file: "/etc/zerofs/checkpoint.toml".into(),
        checkpoint_cache_dir: "/data/zerofs-checkpoint".into(),
    };
    with(&mut settings);
    settings
}

pub fn app_id() -> AppId {
    AppId::parse("app-1").unwrap()
}

pub fn volume_id() -> VolumeId {
    VolumeId::parse("vol-1").unwrap()
}

pub fn deployment_id() -> DeploymentId {
    DeploymentId::parse("dep-1").unwrap()
}

pub fn host_id() -> HostId {
    HostId::parse("host-1").unwrap()
}

pub fn checkpoint_id() -> CheckpointId {
    CheckpointId::parse("chk-1").unwrap()
}

pub fn export_id() -> ExportId {
    ExportId::parse("exp-1").unwrap()
}

pub fn observed_at() -> Timestamp {
    Timestamp::parse(OBSERVED_AT).unwrap()
}

pub fn app_hostname() -> AppHostname {
    AppHostname {
        hostname: Hostname::parse("app-1.apps.example.com").unwrap(),
        kind: AppHostnameKind::Platform,
    }
}

pub fn tenant_environment(values: &[(&str, &str)]) -> TenantEnvironment {
    values
        .iter()
        .map(|(name, value)| (name.to_string(), TenantValue::parse(*value).unwrap()))
        .collect()
}

pub fn artifact(edit: impl FnOnce(&mut DesiredArtifact)) -> DesiredArtifact {
    let mut value = DesiredArtifact {
        digest: Sha256Digest::parse(ARTIFACT_DIGEST).unwrap(),
        size_bytes: ARTIFACT_BYTES.len() as u64,
        object_key: ObjectKey::parse("artifacts/9f1c2f0e-0d4e-4a1b-9c3a-1f8b6d2e7a45").unwrap(),
        filename: Filename::parse("pocketbase").unwrap(),
    };
    edit(&mut value);
    value
}

pub fn app_config(edit: impl FnOnce(&mut AppConfig)) -> AppConfig {
    let mut value = AppConfig {
        http_port: DEFAULT_HTTP_PORT,
        args: TenantArguments::default(),
        environment: TenantEnvironment::default(),
        resources: DEFAULT_INSTANCE_RESOURCES,
        health_check: DEFAULT_HEALTH_CHECK,
        restart_policy: DEFAULT_RESTART_POLICY,
    };
    edit(&mut value);
    value
}

pub fn desired_instance(edit: impl FnOnce(&mut DesiredInstance)) -> DesiredInstance {
    let mut value = DesiredInstance {
        app_id: app_id(),
        deployment_id: deployment_id(),
        volume_id: volume_id(),
        desired_state: DesiredInstanceState::Running,
        idle_timeout_ms: None,
        artifact: artifact(|_| {}),
        config: app_config(|_| {}),
        hostnames: vec![],
    };
    edit(&mut value);
    value
}

pub fn desired_volume(edit: impl FnOnce(&mut DesiredVolume)) -> DesiredVolume {
    let mut value = DesiredVolume {
        volume_id: volume_id(),
        app_id: app_id(),
        size_bytes: VOLUME_SIZE_BYTES,
        desired_state: DesiredPresence::Present,
    };
    edit(&mut value);
    value
}

pub fn desired_checkpoint(edit: impl FnOnce(&mut DesiredCheckpoint)) -> DesiredCheckpoint {
    let mut value = DesiredCheckpoint {
        checkpoint_id: checkpoint_id(),
        volume_id: volume_id(),
        desired_state: DesiredPresence::Present,
    };
    edit(&mut value);
    value
}

pub fn desired_export(edit: impl FnOnce(&mut DesiredExport)) -> DesiredExport {
    let mut value = DesiredExport {
        export_id: export_id(),
        app_id: app_id(),
        volume_id: volume_id(),
        object_key: ObjectKey::parse("exports/app-1/exp-1.tar.gz").unwrap(),
        artifact: artifact(|_| {}),
        environment: Some(TenantEnvironment::default()),
        desired_state: DesiredPresence::Present,
    };
    edit(&mut value);
    value
}

pub fn reported_instance(edit: impl FnOnce(&mut ReportedInstance)) -> ReportedInstance {
    let mut value = ReportedInstance {
        app_id: app_id(),
        deployment_id: deployment_id(),
        state: InstanceState::Running,
        host_port: None,
        guest_ipv4: None,
        artifact_digest: None,
        restart_count: 0,
        started_at: None,
        last_healthy_at: None,
        last_exit_code: None,
        compute: None,
        meters: Default::default(),
        message: None,
    };
    edit(&mut value);
    value
}

pub fn reported_volume(edit: impl FnOnce(&mut ReportedVolume)) -> ReportedVolume {
    let mut value = ReportedVolume {
        volume_id: volume_id(),
        app_id: app_id(),
        state: VolumeState::Ready,
        size_bytes: VOLUME_SIZE_BYTES,
        storage_prefix: None,
        device_path: None,
        usage: None,
        message: None,
    };
    edit(&mut value);
    value
}

pub fn desired_state(edit: impl FnOnce(&mut HostDesiredState)) -> HostDesiredState {
    let mut value = HostDesiredState {
        host_id: host_id(),
        volumes: vec![],
        instances: vec![],
        checkpoints: vec![],
        exports: vec![],
    };
    edit(&mut value);
    value
}

pub fn observed_instance(edit: impl FnOnce(&mut ObservedInstance)) -> ObservedInstance {
    let mut value = ObservedInstance {
        app_id: app_id(),
        volume_id: Some(volume_id()),
        deployment_id: Some(deployment_id()),
        present: true,
        running: true,
        exited: false,
    };
    edit(&mut value);
    value
}

pub fn observed_volume(edit: impl FnOnce(&mut ObservedVolume)) -> ObservedVolume {
    let mut value = ObservedVolume {
        volume_id: volume_id(),
        app_id: app_id(),
        attached: true,
        size_bytes: VOLUME_SIZE_BYTES,
        storage_prefix: ObjectKey::parse(HOST_STORAGE_PREFIX).unwrap(),
        device_path: Some("/dev/nbd0".to_string()),
    };
    edit(&mut value);
    value
}

pub fn observed_state(edit: impl FnOnce(&mut ObservedState)) -> ObservedState {
    let mut value = ObservedState::default();
    edit(&mut value);
    value
}

pub fn record_fields() -> RecordFields {
    let slot = nft_render::describe_slot(nft_render::FIRST_SLOT, app_id());
    RecordFields {
        app_id: app_id(),
        deployment_id: deployment_id(),
        volume_id: volume_id(),
        hostnames: vec![app_hostname()],
        host_port: slot.host_port,
        http_port: DEFAULT_HTTP_PORT,
        guest_ipv4: slot.guest_ipv4,
        artifact_digest: Sha256Digest::parse(ARTIFACT_DIGEST).unwrap(),
        health_check: DEFAULT_HEALTH_CHECK,
        resources: DEFAULT_INSTANCE_RESOURCES,
        desired_running: true,
        on_request: false,
    }
}

pub fn instance_record(edit: impl FnOnce(&mut InstanceRecord)) -> InstanceRecord {
    let mut value = InstanceRecord::new(record_fields(), InstanceState::Running, initial_tracker());
    value.start_attempts = NO_START_ATTEMPTS;
    edit(&mut value);
    value
}

pub static ONE_HOST_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub struct TestHost {
    _directory: tempfile::TempDir,
    pub host: Arc<crate::host::Host>,
    pub vms: mocks::VmmSpy,
    pub commands: mocks::CommandLog,
    pub exports: mocks::ExportSpy,
}

impl TestHost {
    pub fn exports_written(&self) -> Vec<(std::path::PathBuf, protocol::ObjectKey)> {
        self.exports.uploads()
    }
}

impl Deref for TestHost {
    type Target = crate::host::Host;

    fn deref(&self) -> &Self::Target {
        &self.host
    }
}

impl TestHost {
    pub fn arc(&self) -> &Arc<crate::host::Host> {
        &self.host
    }
}

pub async fn test_host() -> TestHost {
    test_host_with(crate::repositories::Repositories::sqlite(
        crate::domain::store::in_memory().await,
    ))
    .await
}

pub async fn test_host_with(repositories: crate::repositories::Repositories) -> TestHost {
    use crate::adapters::net::allocator::SlotAllocator;
    use crate::adapters::net::firewall::HostFirewall;
    use crate::adapters::proxy::activator::AppActivator;
    use crate::adapters::proxy::Router;
    use crate::adapters::volumes::local_file::LocalFileVolumes;
    use crate::config::HostConfig;
    use crate::desired::DesiredStateCache;
    use crate::host::Host;
    use crate::ports::{WakeRefusal, Waker};
    use crate::state::HostState;

    struct NeverWoken;

    #[async_trait::async_trait]
    impl Waker for NeverWoken {
        async fn wake(&self, _app_id: &AppId) -> Result<(), WakeRefusal> {
            Ok(())
        }
    }

    let directory = tempfile::tempdir().expect("a temporary directory");
    let config = HostConfig::under(directory.path());
    let state = HostState::shared();
    let (commands, command_log) = mocks::commands_succeeding();
    let (vms, vm_spy) = mocks::vmm();
    let (exports, export_spy) = mocks::exports_accepting();
    let host = Arc::new(Host {
        guest_memory_mib: u64::from(DEFAULT_INSTANCE_RESOURCES.memory_mib) * 4,
        guest_image_version: "6.1.180-test".to_string(),
        state: state.clone(),
        allocator: Arc::new(Mutex::new(SlotAllocator::empty())),
        cache: Mutex::new(DesiredStateCache::new()),
        vms,
        volumes: Arc::new(LocalFileVolumes::new(
            config.volumes_dir(),
            ObjectKey::parse(&config.storage_prefix).expect("a storage prefix"),
            commands.clone(),
        )),
        artifacts: mocks::artifacts_holding(ARTIFACT_BYTES.to_vec()),
        repositories,
        exports,
        checkpoint_servers: None,
        nbd: crate::adapters::volumes::nbd::NbdDevices::new(commands.clone()),
        commands: commands.clone(),
        firewall: Arc::new(HostFirewall::new(commands.clone())),
        router: Router::new(),
        activator: AppActivator::new(state, Arc::new(NeverWoken)),
        config,
    });
    TestHost {
        _directory: directory,
        host,
        vms: vm_spy,
        commands: command_log,
        exports: export_spy,
    }
}
