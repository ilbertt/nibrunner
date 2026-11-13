use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use protocol::{CronTable, DesiredInstanceState, InstanceState};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinSet;

use crate::controllers::Controller;
use crate::domain::cron::registry::{CronRegistry, CronRegistryError};
use crate::domain::cron::runs::CronRunLease;
use crate::domain::cron::schedule::InvalidCronSchedule;
use crate::domain::cron::scheduler::{CronScheduler, ScheduledRun};
use crate::domain::metrics::start_capacity::Caller;
use crate::host::Host;
use crate::ports::{WakeFailure, WakeRefusal};
use crate::state::GuestActivity;

const CRON_RETRY_DELAY: Duration = Duration::from_secs(1);
const CRON_WAKE_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
enum CronSynchronizationError {
    #[error(transparent)]
    Registry(#[from] CronRegistryError),
    #[error(transparent)]
    Schedule(#[from] InvalidCronSchedule),
}

#[derive(Default)]
struct Scheduling {
    scheduler: CronScheduler,
    tasks: JoinSet<()>,
}

struct RunCompleted(Arc<Notify>);

impl Drop for RunCompleted {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

pub struct CronController {
    host: Arc<Host>,
    registry: Arc<CronRegistry>,
    scheduling: Mutex<Scheduling>,
    completed: Arc<Notify>,
}

impl CronController {
    pub fn new(host: Arc<Host>, registry: Arc<CronRegistry>) -> Arc<Self> {
        Arc::new(Self {
            host,
            registry,
            scheduling: Mutex::new(Scheduling::default()),
            completed: Arc::new(Notify::new()),
        })
    }

    pub async fn cron_once(&self, now: DateTime<Utc>) {
        if let Err(error) = self.try_cron_once(now).await {
            tracing::warn!(%error, "cron registrations could not be synchronized");
        }
    }

    async fn try_cron_once(&self, now: DateTime<Utc>) -> Result<(), CronSynchronizationError> {
        let tables = self.registry.tables().await?;
        let desired = self.host.cache.lock().await.latest().cloned();
        let tables: Vec<_> = tables
            .into_iter()
            .filter(|table| {
                desired.as_ref().is_some_and(|desired| {
                    desired.instances.iter().any(|instance| {
                        instance.app_id == table.app_id
                            && instance.deployment_id == table.deployment_id
                            && instance.desired_state != DesiredInstanceState::Stopped
                    })
                })
            })
            .collect();
        let mut scheduling = self.scheduling.lock().await;
        while let Some(task) = scheduling.tasks.try_join_next() {
            if let Err(error) = task {
                tracing::warn!(%error, "a cron execution task did not finish");
            }
        }
        let time_zone = self.host.config.cron.time_zone;
        scheduling.scheduler.synchronize(&tables, now, time_zone)?;
        for run in scheduling.scheduler.due(now, time_zone) {
            let Some(table) = tables
                .iter()
                .find(|table| table.app_id == run.key.app_id && table.deployment_id == run.key.deployment_id)
                .cloned()
            else {
                continue;
            };
            let Some(lease) = self.host.cron_runs.start(&run.key.app_id, &run.key.deployment_id) else {
                continue;
            };
            let host = self.host.clone();
            let registry = self.registry.clone();
            let completed = self.completed.clone();
            scheduling.tasks.spawn(async move {
                let _completed = RunCompleted(completed);
                Self::execute_owned(host, registry, run, lease, Some(table)).await;
            });
        }
        Ok(())
    }

    async fn current(host: &Host, run: &ScheduledRun) -> bool {
        host.cache.lock().await.latest().is_some_and(|desired| {
            desired.instances.iter().any(|instance| {
                instance.app_id == run.key.app_id
                    && instance.deployment_id == run.key.deployment_id
                    && instance.desired_state != DesiredInstanceState::Stopped
            })
        })
    }

    async fn registered(registry: &CronRegistry, run: &ScheduledRun, expected: Option<&CronTable>) -> bool {
        registry.tables().await.is_ok_and(|tables| {
            tables.iter().any(|table| {
                table.app_id == run.key.app_id
                    && table.deployment_id == run.key.deployment_id
                    && table.jobs.iter().nth(run.key.index) == Some(&run.job)
                    && expected.is_none_or(|expected| table == expected)
            })
        })
    }

    async fn scheduled(
        host: &Host,
        registry: &CronRegistry,
        run: &ScheduledRun,
        expected: Option<&CronTable>,
    ) -> bool {
        Self::current(host, run).await
            && Self::registered(registry, run, expected).await
            && host.state.snapshot().await.isolated
    }

    async fn wake_when_available(
        host: &Host,
        registry: &CronRegistry,
        run: &ScheduledRun,
        expected: Option<&CronTable>,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + CRON_WAKE_QUEUE_TIMEOUT;
        let mut registrations = registry.watch();
        loop {
            if !Self::scheduled(host, registry, run, expected).await {
                return false;
            }
            let available = {
                let _waiting = host.metrics.start_capacity.waiting(Caller::Cron);
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(deadline) => {
                        host.metrics.start_capacity.timed_out(Caller::Cron);
                        tracing::warn!(app_id = %run.key.app_id, "cron timed out waiting for VM start capacity");
                        return false;
                    }
                    change = registrations.changed() => {
                        if change.is_err() { return false; }
                        continue;
                    }
                    available = host.vms.wait_for_start_capacity() => available,
                }
            };
            if let Err(error) = available {
                tracing::warn!(app_id = %run.key.app_id, %error, "cron could not wait for VM start capacity");
                return false;
            }
            if !Self::scheduled(host, registry, run, expected).await {
                return false;
            }
            match host.waker.wake(&run.key.app_id).await {
                Ok(()) => return true,
                // Capacity can be taken by another wake between the notification and admission.
                Err(WakeRefusal::Failed {
                    kind: WakeFailure::StartBusy,
                    ..
                }) => {}
                Err(refusal) => {
                    tracing::warn!(app_id = %run.key.app_id, refusal = ?refusal, "cron could not wake its app");
                    return false;
                }
            }
        }
    }

    async fn prepare(
        host: &Host,
        registry: &CronRegistry,
        run: &ScheduledRun,
        expected: Option<&CronTable>,
    ) -> Option<GuestActivity> {
        let (activity, needs_wake) = {
            let _transition = host.state.transition(&run.key.app_id).await;
            if !Self::scheduled(host, registry, run, expected).await {
                return None;
            }
            let record = host.state.record(&run.key.app_id).await?;
            if record.deployment_id != run.key.deployment_id
                || (record.stop_requested && record.state != InstanceState::Idle)
                || !record.desired_running
                || !matches!(
                    record.state,
                    InstanceState::Idle
                        | InstanceState::Running
                        | InstanceState::Starting
                        | InstanceState::Unhealthy
                )
            {
                return None;
            }
            (
                host.state.guest_activity(&run.key.app_id),
                record.state == InstanceState::Idle,
            )
        };
        if needs_wake && !Self::wake_when_available(host, registry, run, expected).await {
            return None;
        }
        {
            let _transition = host.state.transition(&run.key.app_id).await;
            if !Self::current(host, run).await || !host.state.snapshot().await.isolated {
                return None;
            }
            if !host.state.record(&run.key.app_id).await.is_some_and(|record| {
                record.deployment_id == run.key.deployment_id
                    && record.started_at.is_some()
                    && !record.stop_requested
                    && record.desired_running
                    && matches!(
                        record.state,
                        InstanceState::Running | InstanceState::Starting | InstanceState::Unhealthy
                    )
            }) {
                return None;
            }
        }
        Some(activity)
    }

    async fn execute_owned(
        host: Arc<Host>,
        registry: Arc<CronRegistry>,
        run: ScheduledRun,
        mut lease: CronRunLease,
        expected: Option<CronTable>,
    ) {
        let activity = tokio::select! {
            biased;
            () = lease.cancelled() => None,
            activity = Self::prepare(&host, &registry, &run, expected.as_ref()) => activity,
        };
        if let Some(activity) = activity {
            if let Err(error) = host
                .cron_execution
                .run(
                    &crate::domain::filesystem::reader::guest_vsock_path(&host, &run.key.app_id),
                    run.clone(),
                    Box::pin(lease.cancelled()),
                )
                .await
            {
                tracing::warn!(app_id = %run.key.app_id, deployment_id = %run.key.deployment_id, job_index = run.key.index, %error, "cron execution did not complete");
            }
            host.state
                .mark_active(&run.key.app_id, crate::clock::now_ms())
                .await;
            drop(activity);
        }
    }

    #[cfg(test)]
    async fn dispatch(host: Arc<Host>, registry: Arc<CronRegistry>, run: ScheduledRun) {
        let Some(lease) = host.cron_runs.start(&run.key.app_id, &run.key.deployment_id) else {
            return;
        };
        Self::execute_owned(host, registry, run, lease, None).await;
    }
}

#[async_trait::async_trait]
impl Controller for CronController {
    fn name(&self) -> &'static str {
        "cron"
    }

    async fn run(&self) {
        let mut registrations = self.registry.watch();
        let mut desired = self.host.cache.lock().await.watch();
        loop {
            registrations.borrow_and_update();
            desired.borrow_and_update();
            let retry = if let Err(error) = self.try_cron_once(Utc::now()).await {
                tracing::warn!(%error, "cron registrations could not be synchronized");
                true
            } else {
                false
            };
            let next = self.scheduling.lock().await.scheduler.next_run();
            let timer = async {
                if retry {
                    tokio::time::sleep(CRON_RETRY_DELAY).await;
                } else if let Some(next) = next {
                    let delay = (next - Utc::now()).to_std().unwrap_or_default();
                    tokio::time::sleep(delay).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = timer => {},
                change = registrations.changed() => { if change.is_err() { return; } },
                change = desired.changed() => { if change.is_err() { return; } },
                () = self.completed.notified() => {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::filesystem::reader::guest_vsock_path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use guest_contract::cron_execution::{self as wire, ExecutionFrame};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::*;
    use crate::domain::cron::scheduler::JobKey;
    use crate::domain::store::StoreError;
    use crate::ports::{VmError, Vmm, Waker};
    use crate::repositories::cron_repository::MockCronRepository;
    use crate::test_support::*;

    fn at(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    async fn named_host() -> TestHost {
        let host = test_host().await;
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|_| {})]
        }));
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        host.state
            .put_record(instance_record(|record| {
                record.state = InstanceState::Running;
                record.desired_running = true;
                record.started_at = Some(protocol::Timestamp::now());
            }))
            .await;
        host.cron
            .begin_deployment(&app_id(), &deployment_id())
            .await
            .unwrap();
        host.cron
            .replace(
                &app_id(),
                &deployment_id(),
                "* * * * * echo ok",
                at("2026-01-01T00:00:00Z"),
            )
            .await
            .unwrap();
        host.cron_runs.synchronize(&[(app_id(), deployment_id())]).await;
        host
    }

    fn run() -> ScheduledRun {
        ScheduledRun {
            key: JobKey {
                app_id: app_id(),
                deployment_id: deployment_id(),
                index: 0,
            },
            job: protocol::CronJobDefinition {
                schedule: protocol::CronSchedule::parse("* * * * *").unwrap(),
                command: protocol::CronCommand::parse("echo ok").unwrap(),
                environment: Some(protocol::TenantEnvironment::default()),
            },
            scheduled_at: at("2026-01-01T00:01:00Z"),
        }
    }

    #[tokio::test]
    async fn overlapping_runs_pin_the_guest_and_both_disconnect_before_the_owner_is_stopped() {
        let host = named_host().await;
        let path = guest_vsock_path(&host, &app_id());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let (ready, mut started) = tokio::sync::mpsc::channel(2);
        let guest = tokio::spawn(async move {
            let mut guests = JoinSet::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let ready = ready.clone();
                guests.spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut connect = String::new();
                    stream.read_line(&mut connect).await.unwrap();
                    stream.get_mut().write_all(b"OK 1234\n").await.unwrap();
                    let mut header = [0; wire::HEADER_BYTES];
                    stream.read_exact(&mut header).await.unwrap();
                    let header = wire::decode_request_header(&header).unwrap();
                    let mut body = vec![0; header.body_length];
                    stream.read_exact(&mut body).await.unwrap();
                    stream
                        .get_mut()
                        .write_all(&wire::encode_reply(&ExecutionFrame::Started).unwrap())
                        .await
                        .unwrap();
                    assert_eq!(stream.read_u8().await.unwrap(), wire::ACK);
                    ready.send(()).await.unwrap();
                    assert!(stream.read_u8().await.is_err());
                });
            }
            while let Some(guest) = guests.join_next().await {
                guest.unwrap();
            }
        });
        let controller = CronController::new(host.arc().clone(), host.cron.clone());
        controller.cron_once(at("2026-01-01T00:00:00Z")).await;
        controller.cron_once(at("2026-01-01T00:01:00Z")).await;
        tokio::time::timeout(Duration::from_secs(5), started.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(host.state.has_guest_activity(&app_id()));
        let transition = tokio::time::timeout(Duration::from_secs(1), host.state.transition(&app_id()))
            .await
            .unwrap();
        drop(transition);
        controller.cron_once(at("2026-01-01T00:02:00Z")).await;
        tokio::time::timeout(Duration::from_secs(5), started.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(controller.scheduling.lock().await.tasks.len(), 2);
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::Stopped
            })]
        }));
        host.cron_runs.synchronize(&[]).await;
        tokio::time::timeout(Duration::from_secs(5), guest)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while host.state.has_guest_activity(&app_id()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(host
            .state
            .snapshot()
            .await
            .last_active_at_ms
            .contains_key(&app_id()));
    }

    struct BusyStarts {
        vms: Arc<dyn Vmm>,
        capacity: tokio::sync::Semaphore,
        waiting: Notify,
    }

    #[async_trait::async_trait]
    impl Vmm for BusyStarts {
        async fn wait_for_start_capacity(&self) -> Result<(), VmError> {
            self.waiting.notify_one();
            drop(self.capacity.acquire().await.unwrap());
            Ok(())
        }
        async fn boot(&self, request: crate::ports::BootRequest) -> Result<(), VmError> {
            self.vms.boot(request).await
        }
        async fn sleep(&self, request: crate::ports::SuspendRequest) -> Result<(), VmError> {
            self.vms.sleep(request).await
        }
        async fn wake(&self, request: crate::ports::SuspendRequest) -> Result<(), VmError> {
            self.vms.wake(request).await
        }
        async fn stop(&self, app_id: &protocol::AppId) -> Result<(), VmError> {
            self.vms.stop(app_id).await
        }
        async fn discard(&self, app_id: &protocol::AppId) -> Result<(), VmError> {
            self.vms.discard(app_id).await
        }
        async fn delete_tap(&self, tap_name: &str) -> Result<(), VmError> {
            self.vms.delete_tap(tap_name).await
        }
        async fn tap_names(&self) -> Vec<String> {
            self.vms.tap_names().await
        }
        async fn statuses(
            &self,
            app_ids: &[protocol::AppId],
        ) -> std::collections::BTreeMap<protocol::AppId, crate::adapters::vm::VmStatus> {
            self.vms.statuses(app_ids).await
        }
        async fn adopted_app_ids(&self) -> Vec<protocol::AppId> {
            self.vms.adopted_app_ids().await
        }
        async fn readopt(&self, app_id: &protocol::AppId) -> Result<(), VmError> {
            self.vms.readopt(app_id).await
        }
        async fn guest_verdict(&self, app_id: &protocol::AppId) -> Option<String> {
            self.vms.guest_verdict(app_id).await
        }
        fn working_dir(&self, app_id: &protocol::AppId) -> std::path::PathBuf {
            self.vms.working_dir(app_id)
        }
    }

    async fn queued_host() -> (TestHost, Arc<BusyStarts>, Arc<AtomicBool>) {
        let mut host = named_host().await;
        host.state
            .update_record(&app_id(), |record| {
                record.state = InstanceState::Idle;
                record.started_at = None;
                record.stop_requested = true;
            })
            .await;
        let starts = Arc::new(BusyStarts {
            vms: host.host.vms.clone(),
            capacity: tokio::sync::Semaphore::new(0),
            waiting: Notify::new(),
        });
        let called = Arc::new(AtomicBool::new(false));
        let waker = WakeChecksPin {
            state: host.state.clone(),
            called: called.clone(),
        };
        let inner = Arc::get_mut(&mut host.host).unwrap();
        inner.vms = starts.clone();
        inner.waker = Arc::new(waker);
        (host, starts, called)
    }

    fn capacity_page(host: &Host) -> String {
        crate::domain::metrics::tests::page(
            &crate::domain::metrics::tests::report(),
            &host.metrics,
            &crate::state::HostSnapshot::default(),
            0,
        )
    }

    #[tokio::test]
    async fn a_due_cron_waits_for_start_capacity_without_holding_the_transition_lock() {
        let (host, starts, called) = queued_host().await;
        let dispatch = tokio::spawn(CronController::dispatch(
            host.arc().clone(),
            host.cron.clone(),
            run(),
        ));
        starts.waiting.notified().await;
        assert!(!dispatch.is_finished());
        assert!(capacity_page(&host).contains("nibrunner_vm_start_capacity_waiters{caller=\"cron\"} 1\n"));
        assert!(!called.load(Ordering::SeqCst));
        assert!(host.state.has_guest_activity(&app_id()));
        let transition = tokio::time::timeout(Duration::from_secs(1), host.state.transition(&app_id()))
            .await
            .unwrap();
        drop(transition);
        starts.capacity.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), dispatch)
            .await
            .unwrap()
            .unwrap();
        assert!(called.load(Ordering::SeqCst));
        assert!(!host.state.has_guest_activity(&app_id()));
        assert_eq!(starts.capacity.available_permits(), 1);
        assert!(capacity_page(&host).contains("nibrunner_vm_start_capacity_waiters{caller=\"cron\"} 0\n"));
        assert!(capacity_page(&host)
            .contains("nibrunner_vm_start_capacity_wait_seconds_count{caller=\"cron\"} 1\n"));
    }

    #[tokio::test]
    async fn stopping_a_deployment_cancels_its_queued_wake() {
        let (host, starts, called) = queued_host().await;
        let dispatch = tokio::spawn(CronController::dispatch(
            host.arc().clone(),
            host.cron.clone(),
            run(),
        ));
        starts.waiting.notified().await;
        tokio::time::timeout(Duration::from_secs(5), host.cron_runs.stop(&app_id()))
            .await
            .unwrap();
        dispatch.await.unwrap();
        assert!(!called.load(Ordering::SeqCst));
        assert!(!host.state.has_guest_activity(&app_id()));
    }

    #[tokio::test]
    async fn editing_a_crontab_cancels_its_queued_wake() {
        let (host, starts, called) = queued_host().await;
        let table = host.cron.tables().await.unwrap().remove(0);
        let lease = host.cron_runs.start(&app_id(), &deployment_id()).unwrap();
        let dispatch = tokio::spawn(CronController::execute_owned(
            host.arc().clone(),
            host.cron.clone(),
            run(),
            lease,
            Some(table),
        ));
        starts.waiting.notified().await;
        host.cron
            .replace(
                &app_id(),
                &deployment_id(),
                "# edited\n* * * * * echo ok",
                Utc::now(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), dispatch)
            .await
            .unwrap()
            .unwrap();
        assert!(!called.load(Ordering::SeqCst));
        assert!(!host.state.has_guest_activity(&app_id()));
    }

    #[tokio::test]
    async fn a_queued_wake_expires_without_booting_or_leaking_guest_activity() {
        let (host, starts, called) = queued_host().await;
        let dispatch = tokio::spawn(CronController::dispatch(
            host.arc().clone(),
            host.cron.clone(),
            run(),
        ));
        starts.waiting.notified().await;
        tokio::time::pause();
        tokio::time::advance(CRON_WAKE_QUEUE_TIMEOUT).await;
        dispatch.await.unwrap();
        assert!(!called.load(Ordering::SeqCst));
        assert!(!host.state.has_guest_activity(&app_id()));
        assert_eq!(host.cron_runs.active_for(&app_id()), 0);
        assert!(
            capacity_page(&host).contains("nibrunner_vm_start_capacity_timeouts_total{caller=\"cron\"} 1\n")
        );
        assert!(capacity_page(&host).contains("nibrunner_vm_start_capacity_waiters{caller=\"cron\"} 0\n"));
    }

    struct RefusingWake {
        attempts: AtomicUsize,
        kind: WakeFailure,
        recovery: WakeChecksPin,
    }

    #[async_trait::async_trait]
    impl Waker for RefusingWake {
        async fn wake(&self, app_id: &protocol::AppId) -> Result<(), WakeRefusal> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(WakeRefusal::Failed {
                    kind: self.kind,
                    reason: "refused".into(),
                });
            }
            self.recovery.wake(app_id).await
        }
    }

    #[tokio::test]
    async fn only_busy_admission_is_retried_when_another_start_wins_the_available_slot() {
        for kind in [
            WakeFailure::StartBusy,
            WakeFailure::WouldNotStart,
            WakeFailure::VolumeUnusable,
        ] {
            let (mut host, starts, called) = queued_host().await;
            let waker = Arc::new(RefusingWake {
                attempts: AtomicUsize::new(0),
                kind,
                recovery: WakeChecksPin {
                    state: host.state.clone(),
                    called: called.clone(),
                },
            });
            Arc::get_mut(&mut host.host).unwrap().waker = waker.clone();
            starts.capacity.add_permits(1);
            CronController::dispatch(host.arc().clone(), host.cron.clone(), run()).await;
            let retries = kind == WakeFailure::StartBusy;
            assert_eq!(waker.attempts.load(Ordering::SeqCst), if retries { 2 } else { 1 });
            assert_eq!(called.load(Ordering::SeqCst), retries);
            assert!(!host.state.has_guest_activity(&app_id()));
        }
    }

    struct WakeChecksPin {
        state: crate::state::SharedState,
        called: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Waker for WakeChecksPin {
        async fn wake(&self, app_id: &protocol::AppId) -> Result<(), WakeRefusal> {
            assert!(self.state.has_guest_activity(app_id));
            let _transition = self.state.transition(app_id).await;
            self.state
                .update_record(app_id, |record| {
                    record.state = InstanceState::Running;
                    record.started_at = Some(protocol::Timestamp::now());
                    record.stop_requested = false;
                })
                .await;
            self.called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn guest_activity_is_pinned_before_a_wake_and_released_after_execution_failure() {
        let mut host = named_host().await;
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::OnRequest
            })];
        }));
        host.state
            .update_record(&app_id(), |record| {
                record.state = InstanceState::Idle;
                record.started_at = None;
                record.stop_requested = true;
            })
            .await;
        let called = Arc::new(AtomicBool::new(false));
        let waker = WakeChecksPin {
            state: host.state.clone(),
            called: called.clone(),
        };
        Arc::get_mut(&mut host.host).unwrap().waker = Arc::new(waker);
        tokio::time::timeout(
            Duration::from_secs(5),
            CronController::dispatch(host.arc().clone(), host.cron.clone(), run()),
        )
        .await
        .unwrap();
        assert!(called.load(Ordering::SeqCst));
        assert!(!host.state.has_guest_activity(&app_id()));
    }

    #[tokio::test]
    async fn an_unregistered_or_stale_run_does_not_pin_or_wake_its_guest() {
        let host = named_host().await;
        host.cron
            .replace(&app_id(), &deployment_id(), "", at("2026-01-01T00:00:00Z"))
            .await
            .unwrap();
        CronController::dispatch(host.arc().clone(), host.cron.clone(), run()).await;
        assert!(!host.state.has_guest_activity(&app_id()));
        assert!(host.state.snapshot().await.last_active_at_ms.is_empty());
    }

    #[tokio::test]
    async fn editing_or_removing_the_crontab_keeps_a_started_command_running() {
        let host = named_host().await;
        let path = guest_vsock_path(&host, &app_id());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let (ready, started) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let guest = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut connect = String::new();
            stream.read_line(&mut connect).await.unwrap();
            stream.get_mut().write_all(b"OK 1234\n").await.unwrap();
            let mut header = [0; wire::HEADER_BYTES];
            stream.read_exact(&mut header).await.unwrap();
            let header = wire::decode_request_header(&header).unwrap();
            let mut body = vec![0; header.body_length];
            stream.read_exact(&mut body).await.unwrap();
            stream
                .get_mut()
                .write_all(&wire::encode_reply(&ExecutionFrame::Started).unwrap())
                .await
                .unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), wire::ACK);
            ready.send(()).unwrap();
            tokio::select! {
                _ = finished => {},
                result = stream.read_u8() => panic!("a registry edit interrupted the command: {result:?}"),
            }
            stream
                .get_mut()
                .write_all(&wire::encode_reply(&ExecutionFrame::Exit { code: 0, signal: 0 }).unwrap())
                .await
                .unwrap();
        });
        let dispatch = tokio::spawn(CronController::dispatch(
            host.arc().clone(),
            host.cron.clone(),
            run(),
        ));
        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .unwrap()
            .unwrap();
        for text in ["* * * * * echo replacement", ""] {
            host.cron
                .replace(&app_id(), &deployment_id(), text, at("2026-01-01T00:01:00Z"))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(host.state.has_guest_activity(&app_id()));
        assert!(!dispatch.is_finished());
        finish.send(()).unwrap();
        guest.await.unwrap();
        dispatch.await.unwrap();
        assert!(!host.state.has_guest_activity(&app_id()));
    }

    struct WaitingWake {
        ready: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Waker for WaitingWake {
        async fn wake(&self, _: &protocol::AppId) -> Result<(), WakeRefusal> {
            self.ready.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn stopping_the_owner_cancels_a_waiting_wake_without_waiting_for_readiness() {
        let mut host = named_host().await;
        host.state
            .update_record(&app_id(), |record| {
                record.state = InstanceState::Idle;
                record.stop_requested = true;
            })
            .await;
        let waker = Arc::new(WaitingWake {
            ready: tokio::sync::Notify::new(),
        });
        Arc::get_mut(&mut host.host).unwrap().waker = waker.clone();
        let dispatch = tokio::spawn(CronController::dispatch(
            host.arc().clone(),
            host.cron.clone(),
            run(),
        ));
        tokio::time::timeout(Duration::from_secs(5), waker.ready.notified())
            .await
            .unwrap();
        assert!(host.state.has_guest_activity(&app_id()));
        tokio::time::timeout(Duration::from_secs(5), host.cron_runs.stop(&app_id()))
            .await
            .unwrap();
        dispatch.await.unwrap();
        assert!(!host.state.has_guest_activity(&app_id()));
    }

    #[tokio::test]
    async fn stopping_the_owner_cancels_a_run_waiting_for_the_transition_lock() {
        let host = named_host().await;
        let transition = host.state.transition(&app_id()).await;
        let lease = host.cron_runs.start(&app_id(), &deployment_id()).unwrap();
        let dispatch = tokio::spawn(CronController::execute_owned(
            host.arc().clone(),
            host.cron.clone(),
            run(),
            lease,
            None,
        ));
        tokio::time::timeout(Duration::from_secs(5), host.cron_runs.stop(&app_id()))
            .await
            .unwrap();
        dispatch.await.unwrap();
        assert!(!host.state.has_guest_activity(&app_id()));
        drop(transition);
    }

    #[tokio::test]
    async fn registry_and_desired_notifications_update_timers_without_a_polling_pass() {
        let host = named_host().await;
        host.cron
            .replace(&app_id(), &deployment_id(), "", Utc::now())
            .await
            .unwrap();
        let controller = CronController::new(host.arc().clone(), host.cron.clone());
        let controller_task = tokio::spawn({
            let controller = controller.clone();
            async move { controller.run().await }
        });
        host.cron
            .replace(&app_id(), &deployment_id(), "@yearly echo ok", Utc::now())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.scheduling.lock().await.scheduler.next_run().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::Stopped
            })]
        }));
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.scheduling.lock().await.scheduler.next_run().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        controller_task.abort();
        let _ = controller_task.await;
    }

    #[tokio::test]
    async fn a_comment_edit_after_the_timer_fired_prevents_the_old_table_from_launching() {
        let host = named_host().await;
        let table = host.cron.tables().await.unwrap().remove(0);
        host.cron
            .replace(
                &app_id(),
                &deployment_id(),
                "# edited\n* * * * * echo ok",
                Utc::now(),
            )
            .await
            .unwrap();
        let lease = host.cron_runs.start(&app_id(), &deployment_id()).unwrap();
        CronController::execute_owned(host.arc().clone(), host.cron.clone(), run(), lease, Some(table)).await;
        assert!(!host.state.has_guest_activity(&app_id()));
        assert!(host.state.snapshot().await.last_active_at_ms.is_empty());
    }

    async fn wait_for_reads(reads: &AtomicUsize, observed: &Notify, minimum: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = observed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if reads.load(Ordering::SeqCst) >= minimum {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("the controller attempts registry synchronization in time");
    }

    #[tokio::test]
    async fn a_failed_initial_registry_read_retries_without_a_notification_and_then_stops_retrying() {
        let host = named_host().await;
        host.cron
            .replace(&app_id(), &deployment_id(), "@yearly echo ok", Utc::now())
            .await
            .unwrap();
        let tables = host.cron.tables().await.unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let read = Arc::new(Notify::new());
        let read_observed = read.clone();
        let mut repository = MockCronRepository::new();
        repository.expect_all().returning(move || {
            let attempt = observed.fetch_add(1, Ordering::SeqCst);
            read_observed.notify_one();
            if attempt == 0 {
                Err(StoreError::Unreadable(
                    "the registry is temporarily unavailable".into(),
                ))
            } else {
                Ok(tables.clone())
            }
        });
        let registry = Arc::new(CronRegistry::new(Arc::new(repository), 10, chrono_tz::UTC));
        let controller = CronController::new(host.arc().clone(), registry);
        let running = tokio::spawn({
            let controller = controller.clone();
            async move { controller.run().await }
        });
        wait_for_reads(&reads, &read, 1).await;
        tokio::time::sleep(CRON_RETRY_DELAY / 2).await;
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        wait_for_reads(&reads, &read, 2).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.scheduling.lock().await.scheduler.next_run().is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("a recovered registry restores the timers in time");
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        tokio::time::sleep(CRON_RETRY_DELAY + Duration::from_millis(100)).await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        running.abort();
        let _ = running.await;
    }

    #[tokio::test]
    async fn a_failed_synchronization_waits_for_the_retry_delay_even_with_an_old_due_timer() {
        let host = named_host().await;
        let tables = host.cron.tables().await.unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let read = Arc::new(Notify::new());
        let read_observed = read.clone();
        let mut repository = MockCronRepository::new();
        repository.expect_all().returning(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            read_observed.notify_one();
            Err(StoreError::Unreadable("the registry remains unavailable".into()))
        });
        let registry = Arc::new(CronRegistry::new(Arc::new(repository), 10, chrono_tz::UTC));
        let controller = CronController::new(host.arc().clone(), registry);
        controller
            .scheduling
            .lock()
            .await
            .scheduler
            .synchronize(&tables, Utc::now() - chrono::Duration::minutes(2), chrono_tz::UTC)
            .unwrap();
        let running = tokio::spawn({
            let controller = controller.clone();
            async move { controller.run().await }
        });
        wait_for_reads(&reads, &read, 1).await;
        tokio::time::sleep(CRON_RETRY_DELAY / 2).await;
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        wait_for_reads(&reads, &read, 2).await;
        tokio::time::sleep(CRON_RETRY_DELAY / 2).await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        running.abort();
        let _ = running.await;
    }
}
