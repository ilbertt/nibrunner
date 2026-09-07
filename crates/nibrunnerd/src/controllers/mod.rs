use std::sync::Arc;
use std::time::Duration;

use protocol::HostVersions;

use crate::desired::DesiredStateWatch;
use crate::host::Host;
use crate::services::control_plane::ControlPlaneService;
use crate::services::filesystem::GuestFilesystems;
use crate::services::reconcile::idle::{HostIdle, IdleService};
use crate::services::reconcile::{HostReconciler, ReconcileService};
use crate::services::report::{HostReporter, ReportService};
use crate::services::usage::{HostUsage, UsageService};

const STATUS_TICK: Duration = Duration::from_secs(1);

const SETTLING_TICK: Duration = Duration::from_millis(crate::services::health::STARTUP_PROBE_INTERVAL_MS);

const MIN_REFRESH_GAP: Duration = Duration::from_millis(250);

const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

const IDLE_POLL_FLOOR: Duration = Duration::from_secs(5);

const CONTROL_PLANE_BACKOFF: Duration = Duration::from_secs(15);

pub struct HostLoops {
    host: Arc<Host>,
    reconciler: Arc<dyn ReconcileService>,
    reports: Arc<dyn ReportService>,
    idle: Arc<dyn IdleService>,
    usage: Arc<dyn UsageService>,
}

impl HostLoops {
    pub fn on(host: &Arc<Host>, versions: HostVersions) -> Arc<Self> {
        Arc::new(Self {
            reconciler: HostReconciler::new(host.clone()),
            reports: HostReporter::new(host.clone(), versions),
            idle: HostIdle::new(host.clone()),
            usage: HostUsage::new(host.clone(), GuestFilesystems::new(host.clone())),
            host: host.clone(),
        })
    }

    pub fn from_parts(
        host: Arc<Host>,
        reconciler: Arc<dyn ReconcileService>,
        reports: Arc<dyn ReportService>,
        idle: Arc<dyn IdleService>,
        usage: Arc<dyn UsageService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            reconciler,
            reports,
            idle,
            usage,
        })
    }

    pub async fn converge_cached(&self) -> bool {
        let Some(cached) = self.host.cached_desired_state().await else {
            return false;
        };
        if !self.host.cache.lock().await.accept(cached.clone()) {
            return false;
        }
        self.reconciler.reconcile(&cached).await;
        true
    }

    pub async fn converge_once(&self) -> bool {
        match crate::desired::read_desired_state(&self.host.config.desired_state_file) {
            Ok(Some(desired)) => {
                let news = self.host.cache.lock().await.accept(desired.clone());
                if news {
                    let _ = crate::desired::cache_desired_state(
                        &self.host.config.cached_desired_state_file(),
                        &desired,
                    );
                } else if !self.host.state.snapshot().await.deferred_work {
                    return false;
                }
                self.reconciler.reconcile(&desired).await;
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::error!(error = %error.message(), "the desired state file was not read");
                false
            }
        }
    }

    pub async fn converge_loop(self: Arc<Self>) {
        let watch = DesiredStateWatch::on(&self.host.config.desired_state_file);
        self.converge_cached().await;
        loop {
            self.converge_once().await;
            watch.changed().await;
        }
    }

    pub async fn status_once(&self) -> Duration {
        self.reconciler.refresh().await;
        self.reports.publish().await;

        let now = crate::clock::now_ms();
        let settling = self.host.state.records().await.iter().any(|record| {
            crate::services::health::is_on_startup_grid(&record.health, &record.grace_inputs(now))
        });
        if settling {
            SETTLING_TICK
        } else {
            STATUS_TICK
        }
    }

    pub async fn status_loop(self: Arc<Self>) {
        loop {
            let tick = self.status_once().await;
            tokio::select! {
                _ = tokio::time::sleep(tick) => {}
                _ = async {
                    tokio::time::sleep(MIN_REFRESH_GAP).await;
                    self.host.state.refresh_signalled().await;
                } => {}
            }
        }
    }

    pub async fn measurement_once(&self) {
        self.idle.record_activity().await;
        self.idle.apply_sleep().await;
        self.usage.measure().await;
    }

    pub async fn measurement_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(MEASUREMENT_INTERVAL).await;
            self.measurement_once().await;
        }
    }
}

pub struct ControlPlaneLoops {
    control_plane: Arc<dyn ControlPlaneService>,
}

impl ControlPlaneLoops {
    pub fn on(control_plane: Arc<dyn ControlPlaneService>) -> Arc<Self> {
        Arc::new(Self { control_plane })
    }

    pub async fn poll_once(&self) -> Duration {
        match self.control_plane.poll_desired_state().await {
            Ok(true) => {
                tracing::info!("the control plane gave this host a new document");
                IDLE_POLL_FLOOR
            }
            Ok(false) => IDLE_POLL_FLOOR,
            Err(error) => {
                self.control_plane.note(&error).await;
                tracing::warn!(error = %error.message(), "the control plane could not be polled");
                CONTROL_PLANE_BACKOFF + IDLE_POLL_FLOOR
            }
        }
    }

    pub async fn poll_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(self.poll_once().await).await;
        }
    }

    pub async fn answer_once(&self, spent: Duration) -> Duration {
        match self.control_plane.answer_one_query().await {
            Ok(Some(query)) => {
                tracing::info!(query_id = %query.query_id, app_id = %query.app_id, "a read was answered");
                Duration::ZERO
            }
            Ok(None) => IDLE_POLL_FLOOR.saturating_sub(spent),
            Err(error) => {
                self.control_plane.note(&error).await;
                tracing::warn!(error = %error.message(), "a read could not be collected");
                CONTROL_PLANE_BACKOFF
            }
        }
    }

    pub async fn answer_loop(self: Arc<Self>) {
        loop {
            let started = tokio::time::Instant::now();
            let wait = self.answer_once(started.elapsed()).await;
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::control_plane::ControlPlaneError;
    use crate::services::control_plane::MockControlPlaneService;
    use crate::services::reconcile::idle::MockIdleService;
    use crate::services::reconcile::MockReconcileService;
    use crate::services::report::MockReportService;
    use crate::services::usage::MockUsageService;
    use crate::test_support::*;

    struct Mocks {
        reconciler: MockReconcileService,
        reports: MockReportService,
        idle: MockIdleService,
        usage: MockUsageService,
    }

    impl Mocks {
        fn new() -> Self {
            Self {
                reconciler: MockReconcileService::new(),
                reports: MockReportService::new(),
                idle: MockIdleService::new(),
                usage: MockUsageService::new(),
            }
        }

        fn into_loops(self, host: &TestHost) -> Arc<HostLoops> {
            HostLoops::from_parts(
                host.arc().clone(),
                Arc::new(self.reconciler),
                Arc::new(self.reports),
                Arc::new(self.idle),
                Arc::new(self.usage),
            )
        }
    }

    fn quiet() -> Mocks {
        let mut mocks = Mocks::new();
        mocks.reconciler.expect_reconcile().returning(|_| ());
        mocks.reconciler.expect_refresh().returning(|| ());
        mocks.reports.expect_publish().returning(|| ());
        mocks.idle.expect_record_activity().returning(|| ());
        mocks.idle.expect_apply_sleep().returning(|| ());
        mocks.usage.expect_measure().returning(|| ());
        mocks
    }

    #[tokio::test]
    async fn a_document_that_appeared_is_converged_on() {
        let host = test_host().await;
        let desired = desired_state(|state| state.instances = vec![desired_instance(|_| {})]);
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired).unwrap();

        let mut mocks = Mocks::new();
        let wanted = desired.clone();
        mocks
            .reconciler
            .expect_reconcile()
            .times(1)
            .withf(move |seen| *seen == wanted)
            .returning(|_| ());

        assert!(mocks.into_loops(&host).converge_once().await);
    }

    #[tokio::test]
    async fn a_missing_document_is_the_ordinary_state_of_a_fresh_host() {
        let host = test_host().await;
        let mut mocks = Mocks::new();
        mocks.reconciler.expect_reconcile().never();
        assert!(!mocks.into_loops(&host).converge_once().await);
    }

    #[tokio::test]
    async fn a_document_that_has_not_moved_does_not_run_a_second_pass() {
        let host = test_host().await;
        let desired = desired_state(|_| {});
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired).unwrap();

        let mut mocks = Mocks::new();
        mocks.reconciler.expect_reconcile().times(1).returning(|_| ());
        let loops = mocks.into_loops(&host);
        assert!(loops.converge_once().await);
        assert!(!loops.converge_once().await);
    }

    #[tokio::test]
    async fn work_the_last_pass_deferred_is_carried_even_though_the_document_stood_still() {
        let host = test_host().await;
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired_state(|_| {})).unwrap();

        let mut mocks = Mocks::new();
        mocks.reconciler.expect_reconcile().times(2).returning(|_| ());
        let loops = mocks.into_loops(&host);
        assert!(loops.converge_once().await);
        host.state.modify(|snapshot| snapshot.deferred_work = true).await;
        assert!(loops.converge_once().await);
    }

    #[tokio::test]
    async fn a_document_that_cannot_be_read_is_logged_rather_than_converged_on() {
        let host = test_host().await;
        crate::json_store::make_directory(host.config.desired_state_file.parent().unwrap(), 0o700).unwrap();
        std::fs::write(&host.config.desired_state_file, b"not json at all").unwrap();

        let mut mocks = Mocks::new();
        mocks.reconciler.expect_reconcile().never();
        assert!(!mocks.into_loops(&host).converge_once().await);
    }

    #[tokio::test]
    async fn the_last_document_this_host_was_given_is_what_a_restart_converges_on_first() {
        let host = test_host().await;
        let desired = desired_state(|_| {});
        crate::desired::cache_desired_state(&host.config.cached_desired_state_file(), &desired).unwrap();

        let mut mocks = Mocks::new();
        mocks.reconciler.expect_reconcile().times(1).returning(|_| ());
        assert!(mocks.into_loops(&host).converge_cached().await);
    }

    #[tokio::test]
    async fn a_host_that_was_never_given_a_document_has_nothing_cached_to_converge_on() {
        let host = test_host().await;
        let mut mocks = Mocks::new();
        mocks.reconciler.expect_reconcile().never();
        assert!(!mocks.into_loops(&host).converge_cached().await);
    }

    #[tokio::test]
    async fn a_status_tick_refreshes_before_it_reports_what_it_found() {
        let host = test_host().await;
        let mut sequence = mockall::Sequence::new();
        let mut mocks = Mocks::new();
        mocks
            .reconciler
            .expect_refresh()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());
        mocks
            .reports
            .expect_publish()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());

        assert_eq!(mocks.into_loops(&host).status_once().await, STATUS_TICK);
    }

    #[tokio::test]
    async fn a_host_with_something_still_coming_up_ticks_on_the_faster_grid() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| {
                record.state = protocol::InstanceState::Starting;
                record.started_at = Some(protocol::Timestamp::from_epoch_ms(crate::clock::now_ms()));
            }))
            .await;
        assert_eq!(quiet().into_loops(&host).status_once().await, SETTLING_TICK);
    }

    #[tokio::test]
    async fn a_measurement_sweep_measures_after_it_has_let_the_quiet_apps_go() {
        let host = test_host().await;
        let mut sequence = mockall::Sequence::new();
        let mut mocks = Mocks::new();
        mocks
            .idle
            .expect_record_activity()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());
        mocks
            .idle
            .expect_apply_sleep()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());
        mocks
            .usage
            .expect_measure()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());

        mocks.into_loops(&host).measurement_once().await;
    }

    fn unreachable() -> ControlPlaneError {
        ControlPlaneError::Unreachable {
            route: "/agent/desired-state".into(),
            reason: "connection refused".into(),
        }
    }

    #[tokio::test]
    async fn a_document_the_control_plane_handed_over_waits_out_the_floor_before_asking_again() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_poll_desired_state().returning(|| Ok(true));
        let loops = ControlPlaneLoops::on(Arc::new(control_plane));
        assert_eq!(loops.poll_once().await, IDLE_POLL_FLOOR);
    }

    #[tokio::test]
    async fn a_control_plane_that_would_not_answer_is_backed_off_from_and_the_session_told() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane
            .expect_poll_desired_state()
            .returning(|| Err(unreachable()));
        control_plane.expect_note().times(1).returning(|_| ());

        let loops = ControlPlaneLoops::on(Arc::new(control_plane));
        assert_eq!(loops.poll_once().await, CONTROL_PLANE_BACKOFF + IDLE_POLL_FLOOR);
    }

    #[tokio::test]
    async fn a_read_that_was_answered_is_followed_straight_away_by_asking_for_the_next() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_answer_one_query().returning(|| {
            Ok(Some(protocol::FilesystemQuery {
                query_id: protocol::FilesystemQueryId::parse("q-1").unwrap(),
                app_id: app_id(),
                path: protocol::GuestPath::parse("/").unwrap(),
            }))
        });
        let loops = ControlPlaneLoops::on(Arc::new(control_plane));
        assert_eq!(loops.answer_once(Duration::ZERO).await, Duration::ZERO);
    }

    #[tokio::test]
    async fn a_poll_that_was_held_open_has_already_spent_the_floor_it_would_have_waited() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_answer_one_query().returning(|| Ok(None));
        let loops = ControlPlaneLoops::on(Arc::new(control_plane));
        assert_eq!(loops.answer_once(IDLE_POLL_FLOOR).await, Duration::ZERO);
        assert_eq!(
            loops.answer_once(Duration::from_secs(1)).await,
            IDLE_POLL_FLOOR - Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn a_read_that_could_not_be_collected_backs_off_and_tells_the_session() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane
            .expect_answer_one_query()
            .returning(|| Err(unreachable()));
        control_plane.expect_note().times(1).returning(|_| ());

        let loops = ControlPlaneLoops::on(Arc::new(control_plane));
        assert_eq!(loops.answer_once(Duration::ZERO).await, CONTROL_PLANE_BACKOFF);
    }
}
