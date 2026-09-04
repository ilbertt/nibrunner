//! The fixtures every test in this crate builds from, ported from `apps/agent/tests/support`.
//! Each takes a closure so a test names only the field it is about.

use std::ops::Deref;
use std::sync::Arc;

use protocol::*;
use tokio::sync::Mutex;

use crate::backoff::NO_START_ATTEMPTS;
use crate::health::initial_tracker;
use crate::reconcile::plan::{ObservedInstance, ObservedState, ObservedVolume};
use crate::report::instance_record::{InstanceRecord, RecordFields};

pub const VOLUME_SIZE_BYTES: u64 = 4_096;
pub const OBSERVED_AT: &str = "2026-08-03T10:00:00.000Z";
pub const HOST_STORAGE_PREFIX: &str = "filesystems/host-1";
/// What stands in for a tenant's binary, and the digest of exactly these bytes — asserted in
/// `vm::artifacts`, so the fixture cannot drift from what it claims to be.
pub const ARTIFACT_BYTES: &[u8] = b"#!/usr/bin/env fake-binary\n";
pub const ARTIFACT_DIGEST: &str = "8eacc8ea7f20363ff4eeb79bc80edf5926effee2e7e13207a198ce341a0326f5";

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

/// A tenant's own variables, which are secrets wherever they are typed — including in a test.
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
        // A uuid, as the api will assign: it carries no name, which is why `filename` exists.
        object_key: ObjectKey::parse("artifacts/9f1c2f0e-0d4e-4a1b-9c3a-1f8b6d2e7a45").unwrap(),
        filename: Filename::parse("pocketbase").unwrap(),
    };
    edit(&mut value);
    value
}

pub fn app_config(edit: impl FnOnce(&mut AppConfig)) -> AppConfig {
    let mut value = AppConfig {
        http_port: DEFAULT_HTTP_PORT,
        has_extra_public_port: false,
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
        has_extra_public_port: Some(false),
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

/// A whole host built out of recording services, under a directory that goes away with the test.
/// What it substitutes is everything that would need a hypervisor or a kernel; what it does not
/// substitute is the daemon's own logic, which is the thing being tested.
pub struct TestHost {
    _directory: tempfile::TempDir,
    pub host: Arc<crate::host::Host>,
    pub vms: Arc<crate::services::RecordingVmm>,
    pub commands: Arc<crate::services::RecordingCommandRunner>,
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
    use crate::config::HostConfig;
    use crate::desired::DesiredStateCache;
    use crate::host::Host;
    use crate::net::allocator::SlotAllocator;
    use crate::net::firewall::HostFirewall;
    use crate::proxy::activator::{AppActivator, WakeRefusal, Waker};
    use crate::proxy::Router;
    use crate::services::{RecordingCommandRunner, RecordingVmm, StubArtifactStore};
    use crate::state::HostState;
    use crate::volumes::local_file::LocalFileVolumes;

    /// Nothing here has a microVM to be woken, so a waker that would boot one is not the subject.
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
    let commands = RecordingCommandRunner::succeeding();
    let vms = RecordingVmm::new();
    let host = Arc::new(Host {
        // Room for four apps at the default size, so a test that wants a host with no room says
        // so rather than depending on what the machine running it happens to have.
        guest_memory_mib: u64::from(DEFAULT_INSTANCE_RESOURCES.memory_mib) * 4,
        state: state.clone(),
        allocator: Mutex::new(SlotAllocator::empty()),
        cache: Mutex::new(DesiredStateCache::new()),
        vms: vms.clone(),
        volumes: Arc::new(LocalFileVolumes::new(
            config.volumes_dir(),
            ObjectKey::parse(&config.storage_prefix).expect("a storage prefix"),
            commands.clone(),
        )),
        artifacts: StubArtifactStore::holding(ARTIFACT_BYTES.to_vec()),
        firewall: Arc::new(HostFirewall::new(commands.clone())),
        router: Router::new(),
        activator: AppActivator::new(state, Arc::new(NeverWoken)),
        config,
    });
    TestHost { _directory: directory, host, vms, commands }
}
