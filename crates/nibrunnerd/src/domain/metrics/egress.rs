use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use nft_render::egress::EgressCounts;
use protocol::AppId;

use super::{Kind, Metric, Page};

pub(crate) const INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub(crate) struct Window {
    pub(crate) closed: Instant,
    pub(crate) counts: BTreeMap<AppId, EgressCounts>,
}

const fn gauge(name: &'static str, help: &'static str) -> Metric {
    Metric {
        name,
        help,
        kind: Kind::Gauge,
        labels: &["app"],
    }
}

static CONNECTIONS: Metric = gauge("nibrunner_app_outbound_connections_5m", "New guest TCP or UDP flows admitted through the host filter during the last completed five-minute window.");
static BLOCKED: Metric = gauge(
    "nibrunner_app_outbound_blocked_attempts_5m",
    "New guest TCP or UDP attempts stopped at the host filter during the last completed five-minute window.",
);
static DESTINATIONS: Metric = gauge("nibrunner_app_outbound_destination_ips_5m", "Distinct original destination IPs observed on new attempts in five minutes, capped at 256 per address family.");
static AT_CAPACITY: Metric = gauge(
    "nibrunner_app_outbound_destination_limit_reached_5m",
    "Whether a new destination could not be recorded during the window.",
);

pub(super) static DECLARED: &[&Metric] = &[&CONNECTIONS, &BLOCKED, &DESTINATIONS, &AT_CAPACITY];

pub(super) fn render(page: &mut Page, window: Option<&Window>) {
    for metric in DECLARED {
        page.declare(metric);
    }
    let Some(window) = window.filter(|window| window.closed.elapsed() <= INTERVAL + Duration::from_secs(10))
    else {
        return;
    };
    for (app, row) in &window.counts {
        let labels = [("app", app.as_str())];
        page.value(&CONNECTIONS, &labels, row.passed);
        page.value(&BLOCKED, &labels, row.attempts.saturating_sub(row.passed));
        page.value(&DESTINATIONS, &labels, row.destinations);
        page.value(&AT_CAPACITY, &labels, u8::from(row.unrecorded > 0));
    }
}
