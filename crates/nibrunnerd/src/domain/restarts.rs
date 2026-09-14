use std::sync::Arc;

use async_trait::async_trait;
use protocol::ReportedRestart;

use crate::ports::{LogSink, TenantLogBody, TenantLogEvent};
use crate::state::SharedState;

/// What the guest says about restarting its tenant, taken onto the app's record on the way to
/// the log file: the count `restartCount` and `nibrunner_instance_restarts_total` read, and the
/// reason `lastRestart` carries. The record is the only place the host keeps it — the console
/// file the guest also printed it on is made afresh on every restore.
pub struct RestartRecorder {
    state: SharedState,
    sink: Arc<dyn LogSink>,
}

impl RestartRecorder {
    pub fn new(state: SharedState, sink: Arc<dyn LogSink>) -> Self {
        Self { state, sink }
    }

    async fn record(&self, event: &TenantLogEvent, restart: &protocol::TenantRestart) {
        let mut counted = None;
        self.state
            .update_record(&event.app_id, |record| {
                record.restart_count += 1;
                record.last_restart = Some(ReportedRestart {
                    at: event.observed_at.clone(),
                    restart: restart.clone(),
                });
                counted = Some(record.restart_count);
            })
            .await;
        let Some(restart_count) = counted else {
            tracing::warn!(app_id = %event.app_id, reason = %restart.reason, "a guest this host holds no record of restarted its tenant");
            return;
        };
        tracing::warn!(
            app_id = %event.app_id,
            attempt = restart.attempt,
            budget = restart.budget,
            exit_status = restart.exit.status(),
            backoff_ms = restart.backoff_ms,
            restart_count,
            reason = %restart.reason,
            "the guest restarted its tenant"
        );
        self.state.signal_report();
    }
}

#[async_trait]
impl LogSink for RestartRecorder {
    async fn publish(&self, events: Vec<TenantLogEvent>) {
        for event in &events {
            if let TenantLogBody::Restart(restart) = &event.body {
                self.record(event, restart).await;
            }
        }
        self.sink.publish(events).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::HostState;
    use crate::test_support::mocks;
    use crate::test_support::{
        app_id, deployment_id, instance_record, observed_at, tenant_restart, test_host,
    };
    use protocol::{TenantExit, TenantLogStream};

    fn event(body: TenantLogBody, sequence: u64) -> TenantLogEvent {
        TenantLogEvent {
            app_id: app_id(),
            deployment_id: deployment_id(),
            source_id: "source-1".into(),
            sequence,
            observed_at: observed_at(),
            body,
        }
    }

    fn last_words() -> TenantLogEvent {
        event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stderr,
                text: "out of memory\n".into(),
            },
            0,
        )
    }

    #[tokio::test]
    async fn a_restart_the_guest_reports_is_counted_on_the_record_with_its_reason() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        let (sink, spy) = mocks::log_sink();
        let recorder = RestartRecorder::new(state.clone(), sink);

        let events = vec![
            last_words(),
            event(TenantLogBody::Restart(tenant_restart(|_| {})), 1),
        ];
        recorder.publish(events.clone()).await;

        let record = state.record(&app_id()).await.unwrap();
        assert_eq!(record.restart_count, 1);
        assert_eq!(
            record.last_restart,
            Some(ReportedRestart {
                at: observed_at(),
                restart: tenant_restart(|_| {}),
            })
        );
        assert_eq!(spy.events(), events, "and the log file still gets every line");
    }

    #[tokio::test]
    async fn every_restart_moves_the_count_and_the_latest_reason_is_the_one_kept() {
        let state = HostState::shared();
        state
            .put_record(instance_record(|record| record.restart_count = 3))
            .await;
        let (sink, _) = mocks::log_sink();
        let recorder = RestartRecorder::new(state.clone(), sink);

        recorder
            .publish(vec![event(TenantLogBody::Restart(tenant_restart(|_| {})), 0)])
            .await;
        let later = tenant_restart(|restart| {
            restart.attempt = 2;
            restart.exit = TenantExit::Code(1);
            restart.reason = protocol::StateMessage::new("the tenant exited (1); restart 2 of 5 in 1000ms");
            restart.backoff_ms = 1000;
        });
        recorder
            .publish(vec![event(TenantLogBody::Restart(later.clone()), 1)])
            .await;

        let record = state.record(&app_id()).await.unwrap();
        assert_eq!(record.restart_count, 5);
        assert_eq!(record.last_restart.unwrap().restart, later);
    }

    #[tokio::test]
    async fn what_the_guest_reported_is_what_the_report_and_the_scrape_say() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        let (sink, _) = mocks::log_sink();
        let recorder = RestartRecorder::new(host.state.clone(), sink);

        recorder
            .publish(vec![event(TenantLogBody::Restart(tenant_restart(|_| {})), 0)])
            .await;

        let versions = crate::domain::report::versions::compiled_versions("v1.16.1", "6.1.180-test");
        let report = crate::domain::report::writer::build(&host, versions).await;
        assert_eq!(report.instances[0].restart_count, 1);
        let last = report.instances[0].last_restart.as_ref().unwrap();
        assert_eq!(last.at, observed_at());
        assert_eq!(last.restart, tenant_restart(|_| {}));
        let written = serde_json::to_value(&report).unwrap();
        assert!(written["instances"][0]["lastRestart"]["reason"]
            .as_str()
            .unwrap()
            .contains("running out of memory"));

        let page =
            crate::domain::metrics::tests::page(&report, &host.metrics, &host.state.snapshot().await, 0);
        assert!(
            page.contains("nibrunner_instance_restarts_total{app=\"app-1\"} 1\n"),
            "{page}"
        );
    }

    #[tokio::test]
    async fn output_that_is_not_a_restart_changes_nothing_on_the_record() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        let (sink, spy) = mocks::log_sink();
        let recorder = RestartRecorder::new(state.clone(), sink);

        recorder
            .publish(vec![
                last_words(),
                event(TenantLogBody::Gap { dropped_bytes: 9 }, 1),
            ])
            .await;

        let record = state.record(&app_id()).await.unwrap();
        assert_eq!(record.restart_count, 0);
        assert_eq!(record.last_restart, None);
        assert_eq!(spy.events().len(), 2);
    }

    #[tokio::test]
    async fn a_restart_from_a_guest_this_host_holds_no_record_of_is_logged_and_let_through() {
        let state = HostState::shared();
        let (sink, spy) = mocks::log_sink();
        let recorder = RestartRecorder::new(state.clone(), sink);

        recorder
            .publish(vec![event(TenantLogBody::Restart(tenant_restart(|_| {})), 0)])
            .await;

        assert!(state.records().await.is_empty());
        assert_eq!(spy.events().len(), 1);
    }
}
