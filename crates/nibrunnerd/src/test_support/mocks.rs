use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use protocol::{AppId, ObjectKey};

use crate::adapters::net::tap::{MockHostNetwork, Neighbour, NetworkError, TapInterface};
use crate::adapters::vm::VmStatus;
use crate::domain::exports::store::{ExportStoreError, MockExportStore};
use crate::ports::{
    ArtifactError, CommandError, CommandRequest, CommandResult, MockArtifactStore, MockCommandRunner,
    MockLogSink, MockVmm, TenantLogEvent, VmCall, VmError,
};

fn shared<T>(value: T) -> Arc<Mutex<T>> {
    Arc::new(Mutex::new(value))
}

fn held<T: Clone>(cell: &Arc<Mutex<T>>) -> T {
    cell.lock().expect("no panic holds this lock").clone()
}

fn push<T>(cell: &Arc<Mutex<Vec<T>>>, value: T) {
    cell.lock().expect("no panic holds this lock").push(value);
}

#[derive(Clone, Default)]
pub struct CommandLog {
    calls: Arc<Mutex<Vec<CommandRequest>>>,
}

impl CommandLog {
    pub fn calls(&self) -> Vec<CommandRequest> {
        held(&self.calls)
    }

    pub fn executables(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|request| request.executable().to_string())
            .collect()
    }

    pub fn commands(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().map(|request| request.command).collect()
    }
}

pub fn commands_succeeding() -> (Arc<MockCommandRunner>, CommandLog) {
    commands_answering(|_| Ok(CommandResult::succeeded()))
}

pub fn commands_answering(
    answer: impl Fn(&CommandRequest) -> Result<CommandResult, CommandError> + Send + Sync + 'static,
) -> (Arc<MockCommandRunner>, CommandLog) {
    let log = CommandLog::default();
    let calls = log.calls.clone();
    let mut runner = MockCommandRunner::new();
    runner.expect_run().returning(move |request| {
        push(&calls, request.clone());
        answer(&request)
    });
    (Arc::new(runner), log)
}

#[derive(Clone)]
pub struct VmmSpy {
    calls: Arc<Mutex<Vec<VmCall>>>,
    status: Arc<Mutex<VmStatus>>,
    on_sleep: Arc<Mutex<Option<VmError>>>,
    on_wake: Arc<Mutex<Option<VmError>>>,
    verdict: Arc<Mutex<Option<String>>>,
    adopted: Arc<Mutex<Vec<AppId>>>,
}

impl Default for VmmSpy {
    fn default() -> Self {
        Self {
            calls: shared(Vec::new()),
            status: shared(VmStatus::default()),
            on_sleep: shared(None),
            on_wake: shared(None),
            verdict: shared(None),
            adopted: shared(Vec::new()),
        }
    }
}

impl VmmSpy {
    pub fn calls(&self) -> Vec<VmCall> {
        held(&self.calls)
    }

    pub fn set_status(&self, status: VmStatus) {
        *self.status.lock().expect("no panic holds this lock") = status;
    }

    pub fn refuse_sleep(&self, error: VmError) {
        *self.on_sleep.lock().expect("no panic holds this lock") = Some(error);
    }

    pub fn refuse_wake(&self, error: VmError) {
        *self.on_wake.lock().expect("no panic holds this lock") = Some(error);
    }

    pub fn set_verdict(&self, verdict: impl Into<String>) {
        *self.verdict.lock().expect("no panic holds this lock") = Some(verdict.into());
    }

    pub fn set_adopted(&self, app_ids: Vec<AppId>) {
        *self.adopted.lock().expect("no panic holds this lock") = app_ids;
    }
}

pub const NOWHERE_VM_DIR: &str = "/nowhere/vm";

pub fn vmm() -> (Arc<MockVmm>, VmmSpy) {
    let spy = VmmSpy::default();
    let mut vms = MockVmm::new();

    let calls = spy.calls.clone();
    vms.expect_boot().returning(move |_| {
        push(&calls, VmCall::Boot);
        Ok(())
    });
    let (calls, on_sleep) = (spy.calls.clone(), spy.on_sleep.clone());
    vms.expect_sleep().returning(move |_| {
        push(&calls, VmCall::Sleep);
        held(&on_sleep).map_or(Ok(()), Err)
    });
    let (calls, on_wake) = (spy.calls.clone(), spy.on_wake.clone());
    vms.expect_wake().returning(move |_| {
        push(&calls, VmCall::Wake);
        held(&on_wake).map_or(Ok(()), Err)
    });
    let calls = spy.calls.clone();
    vms.expect_stop().returning(move |_| {
        push(&calls, VmCall::Stop);
        Ok(())
    });
    let calls = spy.calls.clone();
    vms.expect_discard().returning(move |_| {
        push(&calls, VmCall::Discard);
        Ok(())
    });
    let status = spy.status.clone();
    vms.expect_statuses().returning(move |app_ids: &[AppId]| {
        let status = held(&status);
        app_ids.iter().map(|app_id| (app_id.clone(), status)).collect()
    });
    let adopted = spy.adopted.clone();
    vms.expect_adopted_app_ids().returning(move || held(&adopted));
    let verdict = spy.verdict.clone();
    vms.expect_guest_verdict().returning(move |_| held(&verdict));
    vms.expect_working_dir()
        .returning(|app_id: &AppId| PathBuf::from(NOWHERE_VM_DIR).join(app_id.as_str()));

    (Arc::new(vms), spy)
}

pub fn artifacts_holding(bytes: impl Into<Vec<u8>>) -> Arc<MockArtifactStore> {
    let bytes = bytes.into();
    let mut artifacts = MockArtifactStore::new();
    artifacts.expect_read().returning(move |_| Ok(bytes.clone()));
    Arc::new(artifacts)
}

pub fn artifacts_refusing(error: ArtifactError) -> Arc<MockArtifactStore> {
    let mut artifacts = MockArtifactStore::new();
    artifacts.expect_read().returning(move |_| Err(error.clone()));
    Arc::new(artifacts)
}

#[derive(Clone, Default)]
pub struct LogSpy {
    events: Arc<Mutex<Vec<TenantLogEvent>>>,
}

impl LogSpy {
    pub fn events(&self) -> Vec<TenantLogEvent> {
        held(&self.events)
    }
}

pub fn log_sink() -> (Arc<MockLogSink>, LogSpy) {
    let spy = LogSpy::default();
    let events = spy.events.clone();
    let mut sink = MockLogSink::new();
    sink.expect_publish().returning(move |published| {
        events.lock().expect("no panic holds this lock").extend(published);
    });
    (Arc::new(sink), spy)
}

#[derive(Clone, Default)]
pub struct ExportSpy {
    uploads: Arc<Mutex<Vec<(PathBuf, ObjectKey)>>>,
}

impl ExportSpy {
    pub fn uploads(&self) -> Vec<(PathBuf, ObjectKey)> {
        held(&self.uploads)
    }
}

pub fn exports_accepting() -> (Arc<MockExportStore>, ExportSpy) {
    exports_answering(|| Ok(()))
}

pub fn exports_answering(
    answer: impl Fn() -> Result<(), ExportStoreError> + Send + Sync + 'static,
) -> (Arc<MockExportStore>, ExportSpy) {
    let spy = ExportSpy::default();
    let uploads = spy.uploads.clone();
    let mut store = MockExportStore::new();
    store
        .expect_upload()
        .returning(move |bundle_path: &Path, object_key: &ObjectKey| {
            push(&uploads, (bundle_path.to_path_buf(), object_key.clone()));
            answer()
        });
    (Arc::new(store), spy)
}

#[derive(Clone, Default)]
pub struct NetworkSpy {
    taps: Arc<Mutex<Vec<TapInterface>>>,
    neighbours: Arc<Mutex<Vec<Neighbour>>>,
}

impl NetworkSpy {
    pub fn taps(&self) -> Vec<TapInterface> {
        held(&self.taps)
    }

    pub fn neighbours(&self) -> Vec<Neighbour> {
        held(&self.neighbours)
    }
}

pub fn network() -> (Arc<MockHostNetwork>, NetworkSpy) {
    let spy = NetworkSpy::default();
    let mut network = MockHostNetwork::new();

    let taps = spy.taps.clone();
    network.expect_ensure_tap().returning(move |tap: &TapInterface| {
        push(&taps, tap.clone());
        Ok(())
    });
    let neighbours = spy.neighbours.clone();
    network
        .expect_refresh_neighbour()
        .returning(move |neighbour: &Neighbour| {
            push(&neighbours, neighbour.clone());
            Ok(())
        });
    let taps = spy.taps.clone();
    network
        .expect_tap_names()
        .returning(move || held(&taps).into_iter().map(|tap| tap.tap_name).collect());

    (Arc::new(network), spy)
}

pub fn network_refusing(error: NetworkError) -> Arc<MockHostNetwork> {
    let mut network = MockHostNetwork::new();
    let refusal = error.clone();
    network
        .expect_ensure_tap()
        .returning(move |_| Err(refusal.clone()));
    network
        .expect_refresh_neighbour()
        .returning(move |_| Err(error.clone()));
    network.expect_tap_names().returning(Vec::new);
    Arc::new(network)
}

pub type ObservedBackings = BTreeMap<protocol::VolumeId, crate::adapters::volumes::ObservedBacking>;
