use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::Controller;
use crate::host::Host;
use crate::services::reconcile_service::ReconcileService;
use crate::services::report_service::ReportService;

const STATUS_TICK: Duration = Duration::from_secs(1);

const SETTLING_TICK: Duration = Duration::from_millis(crate::domain::health::STARTUP_PROBE_INTERVAL_MS);

const MIN_REFRESH_GAP: Duration = Duration::from_millis(250);

pub struct StatusController {
    host: Arc<Host>,
    reconciler: Arc<dyn ReconcileService>,
    reports: Arc<dyn ReportService>,
}

impl StatusController {
    pub fn new(
        host: Arc<Host>,
        reconciler: Arc<dyn ReconcileService>,
        reports: Arc<dyn ReportService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            reconciler,
            reports,
        })
    }

    pub async fn status_once(&self) -> Duration {
        self.reconciler.refresh().await;
        self.reports.publish().await;

        let now = crate::clock::now_ms();
        let settling = self.host.state.records().await.iter().any(|record| {
            crate::domain::health::is_on_startup_grid(&record.health, &record.grace_inputs(now))
        });
        if settling {
            SETTLING_TICK
        } else {
            STATUS_TICK
        }
    }
}

#[async_trait]
impl Controller for StatusController {
    fn name(&self) -> &'static str {
        "status"
    }

    async fn run(&self) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::reconcile_service::MockReconcileService;
    use crate::services::report_service::MockReportService;
    use crate::test_support::*;

    #[tokio::test]
    async fn a_status_tick_refreshes_before_it_reports_what_it_found() {
        let host = test_host().await;
        let mut sequence = mockall::Sequence::new();
        let mut reconciler = MockReconcileService::new();
        let mut reports = MockReportService::new();
        reconciler
            .expect_refresh()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());
        reports
            .expect_publish()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());

        let controller = StatusController::new(host.arc().clone(), Arc::new(reconciler), Arc::new(reports));
        assert_eq!(controller.status_once().await, STATUS_TICK);
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

        let mut reconciler = MockReconcileService::new();
        let mut reports = MockReportService::new();
        reconciler.expect_refresh().returning(|| ());
        reports.expect_publish().returning(|| ());

        let controller = StatusController::new(host.arc().clone(), Arc::new(reconciler), Arc::new(reports));
        assert_eq!(controller.status_once().await, SETTLING_TICK);
    }
}
