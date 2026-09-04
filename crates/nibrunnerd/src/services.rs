//! The seams. Everything the daemon does that touches the host is behind one of these traits,
//! with one real implementation and one that records what it was asked for — which is what lets
//! the order a deploy does things in be a test rather than a host.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use protocol::{AppId, DeploymentId, DesiredInstance, ObjectKey, Sha256Digest};

use crate::vm::VmStatus;

// ---------------------------------------------------------------------------------------------
// Running a host tool

/// `nft` and `mke2fs` are the only two host tools this daemon spawns besides the Firecracker it
/// carries. Anything else it needs, it does itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub command: Vec<String>,
    pub stdin: Option<String>,
    pub timeout: Duration,
}

impl CommandRequest {
    pub fn new(command: &[&str]) -> Self {
        Self {
            command: command.iter().map(|part| part.to_string()).collect(),
            stdin: None,
            timeout: Duration::from_secs(120),
        }
    }

    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    pub fn executable(&self) -> &str {
        self.command.first().map_or("", String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    pub fn succeeded() -> Self {
        Self {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn with_stdout(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            ..Self::succeeded()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// The tail of stderr: the head of a long one is a banner, and the reason is at the end.
    #[error("{executable} exited {code}{reason}")]
    Failed {
        executable: String,
        code: i32,
        reason: String,
    },
    #[error("{executable} did not finish in time")]
    TimedOut { executable: String },
    #[error("{executable} could not be run: {reason}")]
    Unstartable { executable: String, reason: String },
}

impl CommandError {
    pub fn message(&self) -> String {
        self.to_string()
    }

    pub fn failed(request: &CommandRequest, result: &CommandResult) -> Self {
        let tail = result.stderr.trim().lines().next_back().unwrap_or("").to_string();
        Self::Failed {
            executable: request.executable().to_string(),
            code: result.code,
            reason: if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            },
        }
    }
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, request: CommandRequest) -> Result<CommandResult, CommandError>;

    /// The output of a command that had to succeed.
    async fn stdout_of(&self, request: CommandRequest) -> Result<String, CommandError> {
        let result = self.run(request.clone()).await?;
        if result.code == 0 {
            Ok(result.stdout)
        } else {
            Err(CommandError::failed(&request, &result))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The VMM

#[derive(Debug, Clone)]
pub struct BootRequest {
    pub desired: DesiredInstance,
    pub slot: nft_render::AppSlot,
    pub data_device_path: String,
    pub artifact_image_path: PathBuf,
}

/// What both halves of a suspend need to name the microVM they are acting on.
#[derive(Debug, Clone)]
pub struct SuspendRequest {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub slot: nft_render::AppSlot,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmError {
    #[error("the saved microVM state cannot be restored: {reason}")]
    SnapshotUnusable { reason: String },
    #[error("this microVM must not be snapshotted: {reason}")]
    SleepRefused { reason: String },
    #[error("no microVM answered {socket_path}: {reason}")]
    Unreachable { socket_path: String, reason: String },
    #[error("the microVM refused {path} with {status}{detail}")]
    Rejected {
        path: String,
        status: u16,
        detail: String,
    },
    #[error("{0}")]
    Host(String),
}

impl VmError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// Which of the three ways a wake can end. Returned rather than inferred from the record, because
/// a cold boot and a restore leave the same record behind and only this can tell them apart —
/// which is the difference between the feature working and it quietly not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    Restored,
    AlreadyRunning,
    ColdBoot,
}

impl WakeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeOutcome::Restored => "restored",
            WakeOutcome::AlreadyRunning => "already-running",
            WakeOutcome::ColdBoot => "cold-boot",
        }
    }
}

#[async_trait]
pub trait Vmm: Send + Sync {
    async fn boot(&self, request: BootRequest) -> Result<(), VmError>;
    /// A microVM taken down at a point it can be put back on. Refuses rather than fails where the
    /// guest must not be captured; the caller leaves it running.
    async fn sleep(&self, request: SuspendRequest) -> Result<(), VmError>;
    /// The microVM that went to sleep, back where it was.
    async fn wake(&self, request: SuspendRequest) -> Result<(), VmError>;
    async fn stop(&self, app_id: &AppId) -> Result<(), VmError>;
    /// Everything this host holds for an app, gone: the snapshot, the record of the process, the
    /// working directory and the log attachment.
    async fn discard(&self, app_id: &AppId) -> Result<(), VmError>;
    async fn statuses(&self, app_ids: &[AppId]) -> BTreeMap<AppId, VmStatus>;
    /// Every app this host has a microVM record for, whether or not this daemon started it.
    async fn adopted_app_ids(&self) -> Vec<AppId>;
    /// Why the guest powered itself off, off the console this daemon captured for it.
    async fn guest_verdict(&self, app_id: &AppId) -> Option<String>;
    fn working_dir(&self, app_id: &AppId) -> PathBuf;
}

// ---------------------------------------------------------------------------------------------
// The artifact bucket

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error("the artifact could not be fetched: {0}")]
    Transfer(String),
    #[error("the artifact hashes to {actual}, not to the {expected} it claims")]
    DigestMismatch { expected: Sha256Digest, actual: String },
    #[error("the artifact is {actual} bytes, not the {expected} its manifest declares")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("the artifact image could not be built: {0}")]
    Unpackable(String),
}

impl ArtifactError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// The whole object. Bounded by what an artifact may be rather than streamed, because the
    /// bytes are hashed and packed into an image before anything runs them.
    async fn read(&self, object_key: &ObjectKey) -> Result<Vec<u8>, ArtifactError>;
}

// ---------------------------------------------------------------------------------------------
// Tenant output

/// What the receiver hands the sink, before a host id exists to stamp it with. The daemon learns
/// its host id from a session it may not hold yet when a guest starts writing, and keeping that
/// field off until the record is built is what lets output survive the gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantLogEvent {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub source_id: String,
    pub sequence: u64,
    pub observed_at: protocol::Timestamp,
    pub body: TenantLogBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantLogBody {
    Data {
        stream: protocol::TenantLogStream,
        text: String,
    },
    /// A record rather than a counter, so it lands in the same ordered stream as the output it
    /// replaces: reading the log is how you find out something is missing, and from where.
    Gap { dropped_bytes: u64 },
}

#[async_trait]
pub trait LogSink: Send + Sync {
    async fn publish(&self, events: Vec<TenantLogEvent>);
}

// ---------------------------------------------------------------------------------------------
// Where desired state comes from

/// The three sources produce the same document and the reconciler never learns which one did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredStateOrigin {
    File,
    Socket,
    ControlPlane,
    Cache,
}

impl DesiredStateOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            DesiredStateOrigin::File => "file",
            DesiredStateOrigin::Socket => "socket",
            DesiredStateOrigin::ControlPlane => "control-plane",
            DesiredStateOrigin::Cache => "cache",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Recording implementations

/// Records every command and answers each one, so a call shape can be asserted without a host.
/// What a recorded runner answers one request with.
type CommandAnswer = dyn Fn(&CommandRequest) -> Result<CommandResult, CommandError> + Send + Sync;

pub struct RecordingCommandRunner {
    calls: Mutex<Vec<CommandRequest>>,
    answer: Box<CommandAnswer>,
}

impl RecordingCommandRunner {
    pub fn succeeding() -> Arc<Self> {
        Self::answering(|_| Ok(CommandResult::succeeded()))
    }

    pub fn answering(
        answer: impl Fn(&CommandRequest) -> Result<CommandResult, CommandError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            answer: Box::new(answer),
        })
    }

    pub fn calls(&self) -> Vec<CommandRequest> {
        self.calls.lock().expect("no panic holds this lock").clone()
    }

    pub fn executables(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|request| request.executable().to_string())
            .collect()
    }
}

#[async_trait]
impl CommandRunner for RecordingCommandRunner {
    async fn run(&self, request: CommandRequest) -> Result<CommandResult, CommandError> {
        self.calls
            .lock()
            .expect("no panic holds this lock")
            .push(request.clone());
        (self.answer)(&request)
    }
}

/// Every way the daemon can act on a microVM, recorded rather than performed. Which of them a
/// wake reached is the whole assertion: a restore and a cold boot leave the same app serving, and
/// the only thing that tells them apart from the outside is how long the visitor waited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmCall {
    Boot,
    Sleep,
    Wake,
    Stop,
    Discard,
}

pub struct RecordingVmm {
    calls: Mutex<Vec<VmCall>>,
    status: Mutex<VmStatus>,
    on_sleep: Mutex<Option<VmError>>,
    on_wake: Mutex<Option<VmError>>,
    verdict: Mutex<Option<String>>,
    working_dir: PathBuf,
}

impl RecordingVmm {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            status: Mutex::new(VmStatus::default()),
            on_sleep: Mutex::new(None),
            on_wake: Mutex::new(None),
            verdict: Mutex::new(None),
            working_dir: PathBuf::from("/nowhere/vm"),
        })
    }

    pub fn calls(&self) -> Vec<VmCall> {
        self.calls.lock().expect("no panic holds this lock").clone()
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

    fn record(&self, call: VmCall) {
        self.calls.lock().expect("no panic holds this lock").push(call);
    }
}

#[async_trait]
impl Vmm for RecordingVmm {
    async fn boot(&self, _request: BootRequest) -> Result<(), VmError> {
        self.record(VmCall::Boot);
        Ok(())
    }

    async fn sleep(&self, _request: SuspendRequest) -> Result<(), VmError> {
        self.record(VmCall::Sleep);
        match self.on_sleep.lock().expect("no panic holds this lock").clone() {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }

    async fn wake(&self, _request: SuspendRequest) -> Result<(), VmError> {
        self.record(VmCall::Wake);
        match self.on_wake.lock().expect("no panic holds this lock").clone() {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }

    async fn stop(&self, _app_id: &AppId) -> Result<(), VmError> {
        self.record(VmCall::Stop);
        Ok(())
    }

    async fn discard(&self, _app_id: &AppId) -> Result<(), VmError> {
        self.record(VmCall::Discard);
        Ok(())
    }

    async fn statuses(&self, app_ids: &[AppId]) -> BTreeMap<AppId, VmStatus> {
        let status = *self.status.lock().expect("no panic holds this lock");
        app_ids.iter().map(|app_id| (app_id.clone(), status)).collect()
    }

    async fn adopted_app_ids(&self) -> Vec<AppId> {
        Vec::new()
    }

    async fn guest_verdict(&self, _app_id: &AppId) -> Option<String> {
        self.verdict.lock().expect("no panic holds this lock").clone()
    }

    fn working_dir(&self, app_id: &AppId) -> PathBuf {
        self.working_dir.join(app_id.as_str())
    }
}

/// The artifact bucket as the bytes a transfer is meant to see.
pub struct StubArtifactStore {
    bytes: Vec<u8>,
}

impl StubArtifactStore {
    pub fn holding(bytes: impl Into<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self { bytes: bytes.into() })
    }
}

#[async_trait]
impl ArtifactStore for StubArtifactStore {
    async fn read(&self, _object_key: &ObjectKey) -> Result<Vec<u8>, ArtifactError> {
        Ok(self.bytes.clone())
    }
}

/// Everything a sink was handed, in order.
#[derive(Default)]
pub struct RecordingLogSink {
    events: Mutex<Vec<TenantLogEvent>>,
}

impl RecordingLogSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<TenantLogEvent> {
        self.events.lock().expect("no panic holds this lock").clone()
    }
}

#[async_trait]
impl LogSink for RecordingLogSink {
    async fn publish(&self, events: Vec<TenantLogEvent>) {
        self.events
            .lock()
            .expect("no panic holds this lock")
            .extend(events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_command_that_failed_names_the_tail_of_what_it_said() {
        let runner = RecordingCommandRunner::answering(|_| {
            Ok(CommandResult {
                code: 1,
                stdout: String::new(),
                stderr: "banner\nthe actual reason\n".into(),
            })
        });
        let error = runner
            .stdout_of(CommandRequest::new(&["nft", "-f", "-"]))
            .await
            .unwrap_err();
        assert_eq!(error.message(), "nft exited 1: the actual reason");
        assert_eq!(runner.executables(), vec!["nft"]);
        assert_eq!(runner.calls()[0].command, vec!["nft", "-f", "-"]);
    }

    #[tokio::test]
    async fn a_command_that_succeeded_hands_back_what_it_wrote() {
        let runner = RecordingCommandRunner::answering(|_| Ok(CommandResult::with_stdout("ok")));
        assert_eq!(
            runner.stdout_of(CommandRequest::new(&["nft"])).await.unwrap(),
            "ok"
        );
    }
}
