use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use protocol::{AppId, HostReportedState, INSTANCE_STATES};

// Straddling the millisecond a tenant answers in and the tens of milliseconds a stall costs, since
// telling those apart is the whole reason this histogram exists.
const BUCKET_BOUNDS_SECONDS: [f64; 13] = [
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

#[derive(Debug, Default)]
struct Histogram {
    buckets: [AtomicU64; BUCKET_BOUNDS_SECONDS.len()],
    above_every_bucket: AtomicU64,
    total_micros: AtomicU64,
}

impl Histogram {
    fn record(&self, took: Duration) {
        self.total_micros
            .fetch_add(took.as_micros() as u64, Ordering::Relaxed);
        let seconds = took.as_secs_f64();
        match BUCKET_BOUNDS_SECONDS.iter().position(|bound| seconds <= *bound) {
            Some(bucket) => self.buckets[bucket].fetch_add(1, Ordering::Relaxed),
            None => self.above_every_bucket.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn count(&self) -> u64 {
        self.buckets
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .sum::<u64>()
            + self.above_every_bucket.load(Ordering::Relaxed)
    }

    fn seconds(&self) -> f64 {
        self.total_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

// Per app, deliberately without buckets. A tenant's own distribution would be fifteen series each
// and this page is rendered whole on every scrape; a sum and a count are two, and `rate(sum) /
// rate(count)` is enough to say which tenant is slow. The shape of the distribution is what the
// host-wide histogram beside it is for.
#[derive(Debug, Default, Clone, Copy)]
struct AppTiming {
    count: u64,
    micros: u64,
}

#[derive(Debug, Default)]
pub struct HostMetrics {
    served: [AtomicU64; OUTCOMES.len()],
    answered: Histogram,
    // Not a label on the histogram above: this daemon forwards to a loopback port that either the
    // activator or the guest answers, and nothing comes back to say which. The activator knows,
    // and records here, so the two populations stay separable without a side channel.
    wake: Histogram,
    snapshot: Histogram,
    restore: Histogram,
    per_app: Mutex<BTreeMap<AppId, AppTiming>>,
}

impl HostMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn answered(&self, outcome: Outcome, took: Duration, app_id: Option<&AppId>) {
        let index = OUTCOMES.iter().position(|each| *each == outcome).unwrap_or(0);
        self.served[index].fetch_add(1, Ordering::Relaxed);
        self.answered.record(took);
        let Some(app_id) = app_id else {
            return;
        };
        if let Ok(mut per_app) = self.per_app.lock() {
            let timing = per_app.entry(app_id.clone()).or_default();
            timing.count += 1;
            timing.micros += took.as_micros() as u64;
        }
    }

    pub fn woke(&self, took: Duration) {
        self.wake.record(took);
    }

    pub fn snapshotted(&self, took: Duration) {
        self.snapshot.record(took);
    }

    pub fn restored(&self, took: Duration) {
        self.restore.record(took);
    }

    // An app this host no longer serves keeps no series of its own, or a page rendered whole on
    // every scrape grows for the life of the process with every tenant that ever left.
    pub fn retain_apps(&self, served: &[AppId]) {
        if let Ok(mut per_app) = self.per_app.lock() {
            per_app.retain(|app_id, _| served.contains(app_id));
        }
    }
}

// A label's value is the only place tenant-shaped text reaches a scraper, and the exposition format
// ends a line on a newline and a value on a quote.
fn escaped(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

// Prometheus counts time in seconds and this host meters it in milliseconds. Rendered rather than
// rounded, so a series that is read back and multiplied out is the figure the report carries.
fn as_seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1_000, ms % 1_000)
}

struct Page(String);

impl Page {
    fn new() -> Self {
        Self(String::new())
    }

    fn metric(&mut self, name: &str, help: &str, kind: &str) -> &mut Self {
        self.0
            .push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        self
    }

    fn value(&mut self, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
        if labels.is_empty() {
            self.0.push_str(&format!("{name} {value}\n"));
            return;
        }
        let rendered: Vec<String> = labels
            .iter()
            .map(|(key, value)| format!("{key}=\"{}\"", escaped(value)))
            .collect();
        self.0
            .push_str(&format!("{name}{{{}}} {value}\n", rendered.join(",")));
    }
}

pub fn render(report: &HostReportedState, metrics: &HostMetrics) -> String {
    let mut page = Page::new();

    page.metric(
        "nibrunner_up",
        "Whether this daemon answered the scrape.",
        "gauge",
    );
    page.value("nibrunner_up", &[], 1);

    page.metric(
        "nibrunner_host_capacity",
        "What the host has, by resource.",
        "gauge",
    );
    for (resource, value) in [
        ("vcpu", u64::from(report.capacity.vcpu_count)),
        ("memory_mib", report.capacity.memory_mib),
        ("cache_bytes", report.capacity.cache_bytes),
    ] {
        page.value("nibrunner_host_capacity", &[("resource", resource)], value);
    }

    page.metric(
        "nibrunner_host_allocatable",
        "What the host has left to give a new app.",
        "gauge",
    );
    for (resource, value) in [
        ("vcpu", u64::from(report.allocatable.vcpu_count)),
        ("memory_mib", report.allocatable.memory_mib),
        ("cache_bytes", report.allocatable.cache_bytes),
    ] {
        page.value("nibrunner_host_allocatable", &[("resource", resource)], value);
    }

    // One series per state rather than a number standing for one, so a query reads as the word the
    // report uses and a state nothing is in is a zero rather than a gap.
    page.metric(
        "nibrunner_instance_state",
        "1 for the state an app is in, 0 for every state it is not.",
        "gauge",
    );
    for instance in &report.instances {
        for state in INSTANCE_STATES {
            page.value(
                "nibrunner_instance_state",
                &[("app", instance.app_id.as_str()), ("state", state.as_str())],
                u8::from(instance.state == state),
            );
        }
    }

    page.metric(
        "nibrunner_instance_restarts_total",
        "Times an app's tenant has been restarted by its host.",
        "counter",
    );
    for instance in &report.instances {
        page.value(
            "nibrunner_instance_restarts_total",
            &[("app", instance.app_id.as_str())],
            instance.restart_count,
        );
    }

    page.metric(
        "nibrunner_instance_memory_used_bytes",
        "What a guest reported using, when it was last measured.",
        "gauge",
    );
    for instance in &report.instances {
        if let Some(compute) = &instance.compute {
            page.value(
                "nibrunner_instance_memory_used_bytes",
                &[("app", instance.app_id.as_str())],
                compute.memory_used_bytes,
            );
        }
    }

    page.metric(
        "nibrunner_instance_cpu_share",
        "The share of one vCPU a guest was using, when it was last measured.",
        "gauge",
    );
    for instance in &report.instances {
        if let Some(share) = instance.compute.as_ref().and_then(|compute| compute.cpu_share) {
            page.value(
                "nibrunner_instance_cpu_share",
                &[("app", instance.app_id.as_str())],
                share,
            );
        }
    }

    // The same numbers the report carries, for looking at rather than for billing from: a scrape
    // that was missed is a stretch a rate over these interpolates, and `reported.json` is where
    // the figures nobody may guess at are read from.
    page.metric(
        "nibrunner_instance_time_seconds_total",
        "How long an app has been held, by what it was holding: memory while it runs, a snapshot on disk while it sleeps.",
        "counter",
    );
    for instance in &report.instances {
        for (holding, ms) in [
            ("running", instance.meters.running_ms),
            ("idle", instance.meters.idle_ms),
        ] {
            page.value(
                "nibrunner_instance_time_seconds_total",
                &[("app", instance.app_id.as_str()), ("holding", holding)],
                as_seconds(ms),
            );
        }
    }

    page.metric(
        "nibrunner_instance_cpu_seconds_total",
        "What an app's guest has reported spending, summed across the vCPUs it was given.",
        "counter",
    );
    for instance in &report.instances {
        page.value(
            "nibrunner_instance_cpu_seconds_total",
            &[("app", instance.app_id.as_str())],
            as_seconds(instance.meters.cpu_ms),
        );
    }

    page.metric(
        "nibrunner_instance_network_bytes_total",
        "What has crossed an app's tap, by which way it went. Only what was let out is counted as sent.",
        "counter",
    );
    for instance in &report.instances {
        for (direction, bytes) in [("rx", instance.meters.rx_bytes), ("tx", instance.meters.tx_bytes)] {
            page.value(
                "nibrunner_instance_network_bytes_total",
                &[("app", instance.app_id.as_str()), ("direction", direction)],
                bytes,
            );
        }
    }

    // Mebibyte-seconds rather than bytes, and named for it: what a volume holds is a level, so
    // what accumulates is that level multiplied by how long it was held, and the byte-milliseconds
    // that would be the base-unit form of it outrun a 64-bit counter on a large volume.
    page.metric(
        "nibrunner_instance_disk_mib_seconds_total",
        "What an app has held on disk over time: what was set aside for it, and what its guest reported filling.",
        "counter",
    );
    for instance in &report.instances {
        for (disk, held) in [
            ("provisioned", instance.meters.disk_provisioned_mib_seconds),
            ("used", instance.meters.disk_used_mib_seconds),
        ] {
            page.value(
                "nibrunner_instance_disk_mib_seconds_total",
                &[("app", instance.app_id.as_str()), ("disk", disk)],
                held,
            );
        }
    }

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
    histogram(
        &mut page,
        "nibrunner_proxy_request_duration_seconds",
        &metrics.answered,
    );

    page.metric(
        "nibrunner_wake_duration_seconds",
        "Bringing a sleeping app back for the request that asked for it, from the activator taking the request to the app being ready to be forwarded one. On an on-request host this is the population that makes the tail of the histogram above.",
        "histogram",
    );
    histogram(&mut page, "nibrunner_wake_duration_seconds", &metrics.wake);

    page.metric(
        "nibrunner_instance_snapshot_duration_seconds",
        "Pausing a guest that has gone quiet and writing its memory out. Scales with the memory the app was given, so it is the cost of the sleep rather than of the app.",
        "histogram",
    );
    histogram(
        &mut page,
        "nibrunner_instance_snapshot_duration_seconds",
        &metrics.snapshot,
    );

    page.metric(
        "nibrunner_instance_restore_duration_seconds",
        "Bringing a guest back from the snapshot it slept in. The other half of the sleep, and cheaper than it by orders of magnitude.",
        "histogram",
    );
    histogram(
        &mut page,
        "nibrunner_instance_restore_duration_seconds",
        &metrics.restore,
    );

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

    page.0
}

fn histogram(page: &mut Page, name: &str, held: &Histogram) {
    let mut running = 0u64;
    for (index, bound) in BUCKET_BOUNDS_SECONDS.iter().enumerate() {
        running += held.buckets[index].load(Ordering::Relaxed);
        page.value(&format!("{name}_bucket"), &[("le", &bound.to_string())], running);
    }
    let count = held.count();
    page.value(&format!("{name}_bucket"), &[("le", "+Inf")], count);
    page.value(&format!("{name}_sum"), &[], held.seconds());
    page.value(&format!("{name}_count"), &[], count);
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{
        ComputeUsage, HostCapacity, HostId, HostReportedState, HostState, HostVersions, Timestamp,
    };

    fn report() -> HostReportedState {
        HostReportedState {
            host_id: HostId::parse("host-1").unwrap(),
            reported_at: Timestamp::from_epoch_ms(0),
            state: HostState::Ready,
            capacity: HostCapacity {
                vcpu_count: 12,
                memory_mib: 64_000,
                cache_bytes: 1_000,
            },
            allocatable: HostCapacity {
                vcpu_count: 6,
                memory_mib: 32_000,
                cache_bytes: 500,
            },
            versions: HostVersions {
                agent: "0.1.0".into(),
                guest_image: "test".into(),
                zerofs: "none".into(),
                firecracker: "v1".into(),
            },
            volumes: vec![],
            instances: vec![],
            checkpoints: vec![],
            exports: vec![],
        }
    }

    fn lines_for<'a>(page: &'a str, name: &str) -> Vec<&'a str> {
        page.lines()
            .filter(|line| line.starts_with(name) && !line.starts_with('#'))
            .collect()
    }

    #[test]
    fn a_wake_is_counted_apart_from_the_requests_it_is_the_tail_of() {
        let metrics = HostMetrics::new();
        metrics.answered(Outcome::Served, Duration::from_millis(2), None);
        metrics.answered(Outcome::Served, Duration::from_millis(150), None);
        metrics.woke(Duration::from_millis(148));
        let page = render(&report(), &metrics);

        assert!(page.contains("nibrunner_wake_duration_seconds_count 1"));
        assert!(page.contains("nibrunner_proxy_request_duration_seconds_count 2"));
    }

    #[test]
    fn the_two_halves_of_a_sleep_are_measured_apart_because_they_cost_nothing_alike() {
        let metrics = HostMetrics::new();
        metrics.snapshotted(Duration::from_millis(2563));
        metrics.restored(Duration::from_millis(8));
        let page = render(&report(), &metrics);

        assert!(page.contains("nibrunner_instance_snapshot_duration_seconds_count 1"));
        assert!(page.contains("nibrunner_instance_restore_duration_seconds_count 1"));
        assert!(page.contains("nibrunner_instance_snapshot_duration_seconds_sum 2.563"));
        assert!(page.contains("nibrunner_instance_restore_duration_seconds_sum 0.008"));
    }

    #[test]
    fn what_one_app_cost_is_a_sum_and_a_count_rather_than_a_distribution_of_its_own() {
        let metrics = HostMetrics::new();
        let app = protocol::AppId::parse("app-1").unwrap();
        metrics.answered(Outcome::Served, Duration::from_millis(10), Some(&app));
        metrics.answered(Outcome::Served, Duration::from_millis(30), Some(&app));
        let page = render(&report(), &metrics);

        assert!(page.contains(r#"nibrunner_app_request_duration_seconds_count{app="app-1"} 2"#));
        assert!(page.contains(r#"nibrunner_app_request_duration_seconds_sum{app="app-1"} 0.04"#));
        // Fifteen series an app is what this is instead of, so no bucket may carry one.
        assert!(
            !lines_for(&page, "nibrunner_app_request_duration_seconds_bucket")
                .iter()
                .any(|line| line.contains("app=")),
            "a per-app histogram is the cardinality this metric exists to avoid"
        );
    }

    #[test]
    fn an_app_this_host_has_stopped_serving_keeps_no_series_of_its_own() {
        let metrics = HostMetrics::new();
        let gone = protocol::AppId::parse("app-1").unwrap();
        let kept = protocol::AppId::parse("app-2").unwrap();
        metrics.answered(Outcome::Served, Duration::from_millis(10), Some(&gone));
        metrics.answered(Outcome::Served, Duration::from_millis(10), Some(&kept));

        metrics.retain_apps(std::slice::from_ref(&kept));
        let page = render(&report(), &metrics);

        assert!(!page.contains(r#"app="app-1""#), "a departed app kept a series");
        assert!(page.contains(r#"app="app-2""#));
        // What it cost is still in the host-wide total; only its own line goes.
        assert!(page.contains("nibrunner_proxy_request_duration_seconds_count 2"));
    }

    #[test]
    fn every_series_is_introduced_before_it_is_given_a_value() {
        let page = render(&report(), &HostMetrics::new());
        for line in page
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            let name = line
                .split(['{', ' '])
                .next()
                .unwrap()
                .trim_end_matches("_bucket")
                .trim_end_matches("_sum")
                .trim_end_matches("_count");
            assert!(
                page.contains(&format!("# TYPE {name} ")),
                "{name} is given a value with no TYPE above it"
            );
            assert!(page.contains(&format!("# HELP {name} ")), "{name} has no HELP");
        }
    }

    #[test]
    fn a_histogram_counts_every_request_once_and_its_buckets_only_grow() {
        let proxy = HostMetrics::new();
        for micros in [500, 3_000, 40_000, 90_000_000] {
            proxy.answered(Outcome::Served, Duration::from_micros(micros), None);
        }
        let page = render(&report(), &proxy);

        let counts: Vec<u64> = lines_for(&page, "nibrunner_proxy_request_duration_seconds_bucket")
            .iter()
            .map(|line| line.rsplit(' ').next().unwrap().parse().unwrap())
            .collect();
        assert!(
            counts.windows(2).all(|pair| pair[0] <= pair[1]),
            "buckets are cumulative: {counts:?}"
        );
        assert_eq!(counts.last().copied(), Some(4), "+Inf holds every request");

        let count = lines_for(&page, "nibrunner_proxy_request_duration_seconds_count")[0];
        assert!(count.ends_with(" 4"), "{count}");
        // The one that outran every bound is still in the sum and the count, only not in a bucket.
        assert_eq!(counts[counts.len() - 2], 3);
    }

    #[test]
    fn an_app_is_in_one_state_and_reported_as_not_being_in_the_others() {
        let mut state = report();
        state.instances = vec![crate::test_support::reported_instance(|instance| {
            instance.state = protocol::InstanceState::Idle;
            instance.restart_count = 2;
            instance.compute = Some(ComputeUsage {
                memory_total_bytes: 100,
                memory_used_bytes: 40,
                cpu_share: Some(0.25),
                measured_at: Timestamp::from_epoch_ms(0),
            });
        })];
        let page = render(&state, &HostMetrics::new());

        let states = lines_for(&page, "nibrunner_instance_state");
        assert_eq!(states.len(), INSTANCE_STATES.len());
        assert_eq!(states.iter().filter(|line| line.ends_with(" 1")).count(), 1);
        assert!(states
            .iter()
            .any(|line| line.contains("state=\"idle\"") && line.ends_with(" 1")));
        assert!(lines_for(&page, "nibrunner_instance_restarts_total")[0].ends_with(" 2"));
        assert!(lines_for(&page, "nibrunner_instance_cpu_share")[0].ends_with(" 0.25"));
    }

    #[test]
    fn what_an_app_has_used_is_exposed_as_counters_in_the_units_a_scraper_reads() {
        let mut state = report();
        state.instances = vec![crate::test_support::reported_instance(|instance| {
            instance.meters = protocol::UsageMeters {
                running_ms: 3_600_000,
                idle_ms: 1_500,
                cpu_ms: 42_150,
                rx_bytes: 1_073_741_824,
                tx_bytes: 2_147_483_648,
                disk_provisioned_mib_seconds: 29_491_200,
                disk_used_mib_seconds: 5_242_880,
            };
        })];
        let page = render(&state, &HostMetrics::new());

        let time = lines_for(&page, "nibrunner_instance_time_seconds_total");
        assert_eq!(time.len(), 2, "memory and disk are counted apart: {time:?}");
        assert!(time
            .iter()
            .any(|line| line.contains("holding=\"running\"") && line.ends_with(" 3600.000")));
        assert!(time
            .iter()
            .any(|line| line.contains("holding=\"idle\"") && line.ends_with(" 1.500")));
        assert!(lines_for(&page, "nibrunner_instance_cpu_seconds_total")[0].ends_with(" 42.150"));
        let network = lines_for(&page, "nibrunner_instance_network_bytes_total");
        assert_eq!(network.len(), 2, "each way is counted apart: {network:?}");
        assert!(network
            .iter()
            .any(|line| line.contains("direction=\"rx\"") && line.ends_with(" 1073741824")));
        assert!(network
            .iter()
            .any(|line| line.contains("direction=\"tx\"") && line.ends_with(" 2147483648")));
    }

    #[test]
    fn an_app_that_has_used_nothing_is_a_zero_rather_than_a_missing_series() {
        let mut state = report();
        state.instances = vec![crate::test_support::reported_instance(|_| {})];
        let page = render(&state, &HostMetrics::new());
        for name in [
            "nibrunner_instance_time_seconds_total",
            "nibrunner_instance_cpu_seconds_total",
            "nibrunner_instance_network_bytes_total",
            "nibrunner_instance_disk_mib_seconds_total",
        ] {
            assert!(!lines_for(&page, name).is_empty(), "{name} is missing");
        }
    }

    #[test]
    fn a_label_cannot_end_the_line_it_is_written_on() {
        assert_eq!(escaped(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escaped("a\nb"), "a\\nb");
        assert_eq!(escaped(r"a\b"), r"a\\b");
    }
}
