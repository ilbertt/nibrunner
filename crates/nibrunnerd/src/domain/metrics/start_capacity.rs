use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::{Histogram, Kind, Metric, Page};

const BOUNDS_SECONDS: [f64; 12] = [
    0.0001, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 30.0,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    Http,
    Cron,
}

impl Caller {
    const ALL: [Self; 2] = [Self::Http, Self::Cron];

    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Cron => "cron",
        }
    }
}

#[derive(Debug)]
struct WaitMetrics {
    waiting: AtomicU64,
    duration: Histogram,
    timeouts: AtomicU64,
}

impl Default for WaitMetrics {
    fn default() -> Self {
        Self {
            waiting: AtomicU64::new(0),
            duration: Histogram::over(&BOUNDS_SECONDS),
            timeouts: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Default)]
pub struct StartCapacityMetrics {
    callers: [WaitMetrics; Caller::ALL.len()],
}

impl StartCapacityMetrics {
    fn of(&self, caller: Caller) -> &WaitMetrics {
        let index = Caller::ALL
            .iter()
            .position(|each| *each == caller)
            .expect("every capacity caller is declared");
        &self.callers[index]
    }

    pub fn waiting(&self, caller: Caller) -> CapacityWait<'_> {
        let metrics = self.of(caller);
        metrics.waiting.fetch_add(1, Ordering::Relaxed);
        CapacityWait {
            metrics,
            started: Instant::now(),
        }
    }

    pub fn timed_out(&self, caller: Caller) {
        self.of(caller).timeouts.fetch_add(1, Ordering::Relaxed);
    }
}

#[must_use]
pub struct CapacityWait<'a> {
    metrics: &'a WaitMetrics,
    started: Instant,
}

impl Drop for CapacityWait<'_> {
    fn drop(&mut self) {
        self.metrics.duration.observe(self.started.elapsed());
        self.metrics.waiting.fetch_sub(1, Ordering::Relaxed);
    }
}

static VM_START_CAPACITY_WAIT_SECONDS: Metric = Metric {
    name: "nibrunner_vm_start_capacity_wait_seconds",
    help: "Time spent awaiting each VM start capacity check, by caller. Includes immediately available capacity and cancelled waits; excludes boot, restore and readiness work. A request can wait more than once when admission races.",
    kind: Kind::Histogram,
    labels: &["caller"],
};

static VM_START_CAPACITY_WAITERS: Metric = Metric {
    name: "nibrunner_vm_start_capacity_waiters",
    help: "Callers currently awaiting VM start capacity, excluding those already booting, restoring or checking readiness.",
    kind: Kind::Gauge,
    labels: &["caller"],
};

static VM_START_CAPACITY_TIMEOUTS_TOTAL: Metric = Metric {
    name: "nibrunner_vm_start_capacity_timeouts_total",
    help: "Wake requests whose VM start capacity queue deadline expired, by caller. Cancelled requests and terminal wake failures are not timeouts.",
    kind: Kind::Counter,
    labels: &["caller"],
};

pub(super) static DECLARED: &[&Metric] = &[
    &VM_START_CAPACITY_WAIT_SECONDS,
    &VM_START_CAPACITY_WAITERS,
    &VM_START_CAPACITY_TIMEOUTS_TOTAL,
];

pub(super) fn render(page: &mut Page, metrics: &StartCapacityMetrics) {
    page.declare(&VM_START_CAPACITY_WAIT_SECONDS);
    for (caller, metrics) in Caller::ALL.iter().zip(&metrics.callers) {
        page.histogram(
            &VM_START_CAPACITY_WAIT_SECONDS,
            &[("caller", caller.as_str())],
            &metrics.duration,
        );
    }
    page.declare(&VM_START_CAPACITY_WAITERS);
    for (caller, metrics) in Caller::ALL.iter().zip(&metrics.callers) {
        page.value(
            &VM_START_CAPACITY_WAITERS,
            &[("caller", caller.as_str())],
            metrics.waiting.load(Ordering::Relaxed),
        );
    }
    page.declare(&VM_START_CAPACITY_TIMEOUTS_TOTAL);
    for (caller, metrics) in Caller::ALL.iter().zip(&metrics.callers) {
        page.value(
            &VM_START_CAPACITY_TIMEOUTS_TOTAL,
            &[("caller", caller.as_str())],
            metrics.timeouts.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(metrics: &StartCapacityMetrics) -> String {
        let mut page = Page::new();
        render(&mut page, metrics);
        page.0
    }

    #[test]
    fn concurrent_waits_are_counted_by_caller_and_released_independently() {
        let metrics = StartCapacityMetrics::default();
        let first = metrics.waiting(Caller::Http);
        let second = metrics.waiting(Caller::Http);
        let cron = metrics.waiting(Caller::Cron);
        assert!(page(&metrics).contains("nibrunner_vm_start_capacity_waiters{caller=\"http\"} 2\n"));
        assert!(page(&metrics).contains("nibrunner_vm_start_capacity_waiters{caller=\"cron\"} 1\n"));
        drop(first);
        assert!(page(&metrics).contains("nibrunner_vm_start_capacity_waiters{caller=\"http\"} 1\n"));
        drop(second);
        drop(cron);
        metrics.timed_out(Caller::Cron);
        let page = page(&metrics);
        for caller in Caller::ALL {
            assert!(page.contains(&format!(
                "nibrunner_vm_start_capacity_waiters{{caller=\"{}\"}} 0\n",
                caller.as_str()
            )));
        }
        assert!(page.contains("nibrunner_vm_start_capacity_wait_seconds_count{caller=\"http\"} 2\n"));
        assert!(page.contains("nibrunner_vm_start_capacity_wait_seconds_count{caller=\"cron\"} 1\n"));
        assert!(page.contains("nibrunner_vm_start_capacity_timeouts_total{caller=\"cron\"} 1\n"));
        assert!(page.contains("nibrunner_vm_start_capacity_timeouts_total{caller=\"http\"} 0\n"));
    }

    #[tokio::test]
    async fn cancelling_a_wait_records_its_time_and_releases_its_gauge_without_a_timeout() {
        let metrics = std::sync::Arc::new(StartCapacityMetrics::default());
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let waiting = tokio::spawn({
            let metrics = metrics.clone();
            let entered = entered.clone();
            async move {
                let _waiting = metrics.waiting(Caller::Http);
                entered.notify_one();
                std::future::pending::<()>().await;
            }
        });
        entered.notified().await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        let page = page(&metrics);
        assert!(page.contains("nibrunner_vm_start_capacity_waiters{caller=\"http\"} 0\n"));
        assert!(page.contains("nibrunner_vm_start_capacity_wait_seconds_count{caller=\"http\"} 1\n"));
        assert!(page.contains("nibrunner_vm_start_capacity_timeouts_total{caller=\"http\"} 0\n"));
    }
}
