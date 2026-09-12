use std::time::Duration;

use crate::domain::metrics::proxy::BUCKET_BOUNDS_SECONDS;
use crate::domain::metrics::{Histogram, Page};

/// What sleeping and waking cost: a wake as the activator sees it, and the two halves of a sleep
/// as the VMM does them.
#[derive(Debug)]
pub struct SleepWakeMetrics {
    // Not a label on the request histogram: this daemon forwards to a loopback port that either
    // the activator or the guest answers, and nothing comes back to say which. The activator
    // knows, and records here, so the two populations stay separable without a side channel.
    wake: Histogram,
    snapshot: Histogram,
    restore: Histogram,
}

impl Default for SleepWakeMetrics {
    fn default() -> Self {
        Self {
            wake: Histogram::over(&BUCKET_BOUNDS_SECONDS),
            snapshot: Histogram::over(&BUCKET_BOUNDS_SECONDS),
            restore: Histogram::over(&BUCKET_BOUNDS_SECONDS),
        }
    }
}

impl SleepWakeMetrics {
    pub fn woke(&self, took: Duration) {
        self.wake.observe(took);
    }

    pub fn snapshotted(&self, took: Duration) {
        self.snapshot.observe(took);
    }

    pub fn restored(&self, took: Duration) {
        self.restore.observe(took);
    }
}

pub(super) fn render(page: &mut Page, metrics: &SleepWakeMetrics) {
    page.metric(
        "nibrunner_wake_duration_seconds",
        "Bringing a sleeping app back for the request that asked for it, from the activator taking the request to the app being ready to be forwarded one. On an on-request host this is the population that makes the tail of nibrunner_proxy_request_duration_seconds.",
        "histogram",
    );
    page.histogram("nibrunner_wake_duration_seconds", &[], &metrics.wake);

    page.metric(
        "nibrunner_instance_snapshot_duration_seconds",
        "Pausing a guest that has gone quiet and writing its memory out. Scales with the memory the app was given, so it is the cost of the sleep rather than of the app.",
        "histogram",
    );
    page.histogram(
        "nibrunner_instance_snapshot_duration_seconds",
        &[],
        &metrics.snapshot,
    );

    page.metric(
        "nibrunner_instance_restore_duration_seconds",
        "Bringing a guest back from the snapshot it slept in. The other half of the sleep, and cheaper than it by orders of magnitude.",
        "histogram",
    );
    page.histogram(
        "nibrunner_instance_restore_duration_seconds",
        &[],
        &metrics.restore,
    );
}
