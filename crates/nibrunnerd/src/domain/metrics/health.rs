use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use protocol::{AppId, HostReportedState};

use crate::domain::metrics::{as_seconds, Histogram, Page};
use crate::state::HostSnapshot;

// A probe that answers does so in a millisecond; one that does not runs out the check's timeout,
// two seconds by default.
const PROBE_BOUNDS_SECONDS: [f64; 11] = [0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5];

/// Why an instance failed, by where on the way up it did: the word a counter uses, where the
/// record's message carries the sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Refused,
    NoSlot,
    Layers,
    Volume,
    Boot,
    Exited,
    NeverAnswered,
    OutOfRestarts,
}

impl Failure {
    pub fn as_str(self) -> &'static str {
        match self {
            Failure::Refused => "refused",
            Failure::NoSlot => "no_slot",
            Failure::Layers => "layers",
            Failure::Volume => "volume",
            Failure::Boot => "boot",
            Failure::Exited => "exited",
            Failure::NeverAnswered => "never_answered",
            Failure::OutOfRestarts => "out_of_restarts",
        }
    }
}

const FAILURES: [Failure; 8] = [
    Failure::Refused,
    Failure::NoSlot,
    Failure::Layers,
    Failure::Volume,
    Failure::Boot,
    Failure::Exited,
    Failure::NeverAnswered,
    Failure::OutOfRestarts,
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AppHealth {
    pub probes_healthy: u64,
    pub probes_unhealthy: u64,
    pub failures: [u64; FAILURES.len()],
    pub went_unhealthy: u64,
}

fn position<T: PartialEq>(of: &[T], value: &T) -> usize {
    of.iter().position(|each| each == value).unwrap_or(0)
}

/// What the host has seen of each app's health: every probe and its answer, every time an app
/// failed and where, every time one that had been well stopped answering.
#[derive(Debug)]
pub struct HealthMetrics {
    probe_healthy: Histogram,
    probe_unhealthy: Histogram,
    apps: Mutex<BTreeMap<AppId, AppHealth>>,
}

impl Default for HealthMetrics {
    fn default() -> Self {
        Self {
            probe_healthy: Histogram::over(&PROBE_BOUNDS_SECONDS),
            probe_unhealthy: Histogram::over(&PROBE_BOUNDS_SECONDS),
            apps: Mutex::new(BTreeMap::new()),
        }
    }
}

impl HealthMetrics {
    fn app(&self, app_id: &AppId, change: impl FnOnce(&mut AppHealth)) {
        let mut apps = self.apps.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        change(apps.entry(app_id.clone()).or_default());
    }

    pub fn probed(&self, app_id: &AppId, healthy: bool, took: Duration) {
        if healthy {
            self.probe_healthy.observe(took);
        } else {
            self.probe_unhealthy.observe(took);
        }
        self.app(app_id, |app| {
            if healthy {
                app.probes_healthy += 1;
            } else {
                app.probes_unhealthy += 1;
            }
        });
    }

    pub fn failed(&self, app_id: &AppId, failure: Failure) {
        self.app(app_id, |app| app.failures[position(&FAILURES, &failure)] += 1);
    }

    pub fn went_unhealthy(&self, app_id: &AppId) {
        self.app(app_id, |app| app.went_unhealthy += 1);
    }

    pub fn forget(&self, app_id: &AppId) {
        self.apps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(app_id);
    }

    pub fn of(&self, app_id: &AppId) -> AppHealth {
        self.apps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(app_id)
            .cloned()
            .unwrap_or_default()
    }
}

pub(super) fn render(
    page: &mut Page,
    report: &HostReportedState,
    metrics: &HealthMetrics,
    snapshot: &HostSnapshot,
) {
    page.metric(
        "nibrunner_health_probe_seconds",
        "A health probe, by what it found. One that found nothing ran out the check's timeout.",
        "histogram",
    );
    page.histogram(
        "nibrunner_health_probe_seconds",
        &[("result", "healthy")],
        &metrics.probe_healthy,
    );
    page.histogram(
        "nibrunner_health_probe_seconds",
        &[("result", "unhealthy")],
        &metrics.probe_unhealthy,
    );

    page.metric(
        "nibrunner_instance_health_probes_total",
        "Probes of an app, by what they found. For an app that promised only to boot, a probe is the microVM being up.",
        "counter",
    );
    for instance in &report.instances {
        let app = metrics.of(&instance.app_id);
        for (result, count) in [
            ("healthy", app.probes_healthy),
            ("unhealthy", app.probes_unhealthy),
        ] {
            page.value(
                "nibrunner_instance_health_probes_total",
                &[("app", instance.app_id.as_str()), ("result", result)],
                count,
            );
        }
    }

    page.metric(
        "nibrunner_instance_failures_total",
        "Times an app failed, by where on the way up it did: refused by the document, no slot, its layers, its volume, the boot, the microVM stopping without being asked, nothing answering inside it, or its restarts running out.",
        "counter",
    );
    for instance in &report.instances {
        let app = metrics.of(&instance.app_id);
        for (index, failure) in FAILURES.iter().enumerate() {
            page.value(
                "nibrunner_instance_failures_total",
                &[("app", instance.app_id.as_str()), ("reason", failure.as_str())],
                app.failures[index],
            );
        }
    }

    page.metric(
        "nibrunner_instance_unhealthy_total",
        "Times an app that had been answering stopped, for long enough to be called unhealthy.",
        "counter",
    );
    for instance in &report.instances {
        page.value(
            "nibrunner_instance_unhealthy_total",
            &[("app", instance.app_id.as_str())],
            metrics.of(&instance.app_id).went_unhealthy,
        );
    }

    page.metric(
        "nibrunner_instance_last_healthy_timestamp_seconds",
        "When an app last answered a probe, as seconds since the epoch. 0 for one that never has.",
        "gauge",
    );
    for instance in &report.instances {
        page.value(
            "nibrunner_instance_last_healthy_timestamp_seconds",
            &[("app", instance.app_id.as_str())],
            as_seconds(
                instance
                    .last_healthy_at
                    .as_ref()
                    .map_or(0, |at| at.epoch_ms().max(0) as u64),
            ),
        );
    }

    page.metric(
        "nibrunner_instance_start_attempts",
        "Starts attempted against an app's restart budget in its current window. Resets when a start has held for the policy's resetAfterMs.",
        "gauge",
    );
    for record in snapshot.records.values() {
        page.value(
            "nibrunner_instance_start_attempts",
            &[("app", record.app_id.as_str())],
            record.start_attempts.attempts,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::metrics::tests::page;
    use crate::domain::metrics::HostMetrics;
    use crate::test_support::*;
    use protocol::Timestamp;

    fn lines_for<'a>(page: &'a str, name: &str) -> Vec<&'a str> {
        page.lines()
            .filter(|line| line.starts_with(&format!("{name}{{")) || line.starts_with(&format!("{name} ")))
            .collect()
    }

    #[tokio::test]
    async fn every_probe_failure_and_lapse_of_an_app_is_counted_on_it() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| {
                record.start_attempts.attempts = 3;
            }))
            .await;
        let metrics = &host.metrics.health;
        metrics.probed(&app_id(), true, Duration::from_millis(1));
        metrics.probed(&app_id(), true, Duration::from_millis(1));
        metrics.probed(&app_id(), false, Duration::from_secs(2));
        metrics.failed(&app_id(), Failure::Exited);
        metrics.failed(&app_id(), Failure::Exited);
        metrics.failed(&app_id(), Failure::OutOfRestarts);
        metrics.went_unhealthy(&app_id());

        let mut report = crate::domain::metrics::tests::report();
        report.instances = vec![reported_instance(|instance| {
            instance.last_healthy_at = Some(Timestamp::from_epoch_ms(1_700_000_000_250));
        })];
        let page = page(&report, &host.metrics, &host.state.snapshot().await, 0);

        assert_eq!(
            lines_for(&page, "nibrunner_instance_health_probes_total"),
            vec![
                "nibrunner_instance_health_probes_total{app=\"app-1\",result=\"healthy\"} 2",
                "nibrunner_instance_health_probes_total{app=\"app-1\",result=\"unhealthy\"} 1",
            ]
        );
        assert!(page.contains("nibrunner_health_probe_seconds_count{result=\"healthy\"} 2\n"));
        assert!(page.contains("nibrunner_health_probe_seconds_count{result=\"unhealthy\"} 1\n"));
        let failures = lines_for(&page, "nibrunner_instance_failures_total");
        assert_eq!(
            failures.len(),
            FAILURES.len(),
            "every reason is there to be rated from zero"
        );
        assert!(failures.contains(&"nibrunner_instance_failures_total{app=\"app-1\",reason=\"exited\"} 2"));
        assert!(failures
            .contains(&"nibrunner_instance_failures_total{app=\"app-1\",reason=\"out_of_restarts\"} 1"));
        assert!(failures.contains(&"nibrunner_instance_failures_total{app=\"app-1\",reason=\"boot\"} 0"));
        assert!(page.contains("nibrunner_instance_unhealthy_total{app=\"app-1\"} 1\n"));
        assert!(page
            .contains("nibrunner_instance_last_healthy_timestamp_seconds{app=\"app-1\"} 1700000000.250\n"));
        assert!(page.contains("nibrunner_instance_start_attempts{app=\"app-1\"} 3\n"));
    }

    #[test]
    fn an_app_that_never_answered_reads_as_never_rather_than_as_a_gap() {
        let metrics = HostMetrics::default();
        let mut report = crate::domain::metrics::tests::report();
        report.instances = vec![reported_instance(|_| {})];
        let page = page(&report, &metrics, &HostSnapshot::default(), 0);
        assert!(page.contains("nibrunner_instance_last_healthy_timestamp_seconds{app=\"app-1\"} 0.000\n"));
    }

    #[test]
    fn an_app_the_document_dropped_is_forgotten() {
        let metrics = HealthMetrics::default();
        metrics.failed(&app_id(), Failure::Boot);
        assert_eq!(
            metrics.of(&app_id()).failures[position(&FAILURES, &Failure::Boot)],
            1
        );
        metrics.forget(&app_id());
        assert_eq!(metrics.of(&app_id()), AppHealth::default());
    }
}
