use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use protocol::AppId;

use crate::domain::metrics::{Histogram, Page};

// Straddling the millisecond a tenant answers in and the tens of milliseconds a stall costs, since
// telling those apart is the whole reason this histogram exists.
pub(super) const BUCKET_BOUNDS_SECONDS: [f64; 13] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Served,
    NoSuchHost,
    WrongHost,
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Served => "served",
            Outcome::NoSuchHost => "no_such_host",
            Outcome::WrongHost => "wrong_host",
            Outcome::Refused => "refused",
        }
    }
}

// What this proxy decided, which is all it can honestly claim: a 502 or a 503 travelling back
// through it was the upstream's answer and is counted as served, because serving it is what
// happened.
const OUTCOMES: [Outcome; 4] = [
    Outcome::Served,
    Outcome::NoSuchHost,
    Outcome::WrongHost,
    Outcome::Refused,
];

// Per app, deliberately without buckets. A tenant's own distribution would be fifteen series each
// and this page is rendered whole on every scrape; a sum and a count are two, and `rate(sum) /
// rate(count)` is enough to say which tenant is slow. The shape of the distribution is what the
// host-wide histogram beside it is for.
#[derive(Debug, Default, Clone, Copy)]
struct AppTiming {
    count: u64,
    micros: u64,
}

#[derive(Debug)]
pub struct ProxyMetrics {
    served: [AtomicU64; OUTCOMES.len()],
    answered: Histogram,
    per_app: Mutex<BTreeMap<AppId, AppTiming>>,
}

impl Default for ProxyMetrics {
    fn default() -> Self {
        Self {
            served: Default::default(),
            answered: Histogram::over(&BUCKET_BOUNDS_SECONDS),
            per_app: Mutex::new(BTreeMap::new()),
        }
    }
}

impl ProxyMetrics {
    pub fn answered(&self, outcome: Outcome, took: Duration, app_id: Option<&AppId>) {
        let index = OUTCOMES.iter().position(|each| *each == outcome).unwrap_or(0);
        self.served[index].fetch_add(1, Ordering::Relaxed);
        self.answered.observe(took);
        let Some(app_id) = app_id else {
            return;
        };
        if let Ok(mut per_app) = self.per_app.lock() {
            let timing = per_app.entry(app_id.clone()).or_default();
            timing.count += 1;
            timing.micros += took.as_micros() as u64;
        }
    }

    // An app this host no longer serves keeps no series of its own, or a page rendered whole on
    // every scrape grows for the life of the process with every tenant that ever left.
    pub fn retain_apps(&self, served: &[AppId]) {
        if let Ok(mut per_app) = self.per_app.lock() {
            per_app.retain(|app_id, _| served.contains(app_id));
        }
    }
}

pub(super) fn render(page: &mut Page, metrics: &ProxyMetrics) {
    page.metric(
        "nibrunner_proxy_requests_total",
        "Requests this proxy answered, by what it answered with.",
        "counter",
    );
    for (index, outcome) in OUTCOMES.iter().enumerate() {
        page.value(
            "nibrunner_proxy_requests_total",
            &[("outcome", outcome.as_str())],
            metrics.served[index].load(Ordering::Relaxed),
        );
    }

    page.metric(
        "nibrunner_proxy_request_duration_seconds",
        "Deciding a route and getting an answer back from the app. Ends when the response is handed on to be written, so it counts neither the handshake before it nor the write after.",
        "histogram",
    );
    page.histogram("nibrunner_proxy_request_duration_seconds", &[], &metrics.answered);

    // A summary with no quantiles, which is a sum and a count: what each tenant cost, without
    // fifteen series apiece for the shape of it.
    page.metric(
        "nibrunner_app_request_duration_seconds",
        "What requests to one app cost this proxy. Divide the rate of the sum by the rate of the count for that app's mean.",
        "summary",
    );
    if let Ok(per_app) = metrics.per_app.lock() {
        for (app_id, timing) in per_app.iter() {
            page.value(
                "nibrunner_app_request_duration_seconds_sum",
                &[("app", app_id.as_str())],
                timing.micros as f64 / 1_000_000.0,
            );
            page.value(
                "nibrunner_app_request_duration_seconds_count",
                &[("app", app_id.as_str())],
                timing.count,
            );
        }
    }
}
