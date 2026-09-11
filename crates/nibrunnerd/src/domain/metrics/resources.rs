use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use protocol::{CheckpointState, ExportState, VolumeState};

use crate::domain::metrics::{as_seconds, Histogram, Page, Scrape};

// A local-file volume is made in milliseconds; one on an object store is made in seconds and
// exported in minutes, and a layer pulled over a slow link in more.
const BUCKET_BOUNDS_SECONDS: [f64; 14] = [
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0,
];

/// Something done to storage on an app's behalf, each timed apart because each is a different
/// backend call with a different reason to be slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    VolumeProvision,
    VolumeAttach,
    VolumeTeardown,
    CheckpointCreate,
    CheckpointDelete,
    ExportWrite,
    LayerFetch,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::VolumeProvision => "volume_provision",
            Operation::VolumeAttach => "volume_attach",
            Operation::VolumeTeardown => "volume_teardown",
            Operation::CheckpointCreate => "checkpoint_create",
            Operation::CheckpointDelete => "checkpoint_delete",
            Operation::ExportWrite => "export_write",
            Operation::LayerFetch => "layer_fetch",
        }
    }
}

const OPERATIONS: [Operation; 7] = [
    Operation::VolumeProvision,
    Operation::VolumeAttach,
    Operation::VolumeTeardown,
    Operation::CheckpointCreate,
    Operation::CheckpointDelete,
    Operation::ExportWrite,
    Operation::LayerFetch,
];

const OUTCOMES: [&str; 2] = ["ok", "failed"];

const VOLUME_STATES: [VolumeState; 5] = [
    VolumeState::Pending,
    VolumeState::Ready,
    VolumeState::Detached,
    VolumeState::Deleted,
    VolumeState::Failed,
];

const CHECKPOINT_STATES: [CheckpointState; 3] = [
    CheckpointState::Pending,
    CheckpointState::Ready,
    CheckpointState::Failed,
];

const EXPORT_STATES: [ExportState; 5] = [
    ExportState::Pending,
    ExportState::Preparing,
    ExportState::Ready,
    ExportState::Failed,
    ExportState::Expired,
];

fn volume_state_str(state: VolumeState) -> &'static str {
    match state {
        VolumeState::Pending => "pending",
        VolumeState::Ready => "ready",
        VolumeState::Detached => "detached",
        VolumeState::Deleted => "deleted",
        VolumeState::Failed => "failed",
    }
}

fn checkpoint_state_str(state: CheckpointState) -> &'static str {
    match state {
        CheckpointState::Pending => "pending",
        CheckpointState::Ready => "ready",
        CheckpointState::Failed => "failed",
    }
}

fn export_state_str(state: ExportState) -> &'static str {
    match state {
        ExportState::Pending => "pending",
        ExportState::Preparing => "preparing",
        ExportState::Ready => "ready",
        ExportState::Failed => "failed",
        ExportState::Expired => "expired",
    }
}

fn position<T: PartialEq>(of: &[T], value: &T) -> usize {
    of.iter().position(|each| each == value).unwrap_or(0)
}

/// What storage and the artifact store cost this host: every operation timed by how it ended,
/// and what the layer cache saved.
#[derive(Debug)]
pub struct ResourceMetrics {
    operations: Vec<Histogram>,
    layers_cached: AtomicU64,
    layer_fetch_bytes: AtomicU64,
}

impl Default for ResourceMetrics {
    fn default() -> Self {
        Self {
            operations: (0..OPERATIONS.len() * OUTCOMES.len())
                .map(|_| Histogram::over(&BUCKET_BOUNDS_SECONDS))
                .collect(),
            layers_cached: AtomicU64::new(0),
            layer_fetch_bytes: AtomicU64::new(0),
        }
    }
}

impl ResourceMetrics {
    fn operation(&self, operation: Operation, ok: bool) -> &Histogram {
        &self.operations[position(&OPERATIONS, &operation) * OUTCOMES.len() + usize::from(!ok)]
    }

    pub fn done(&self, operation: Operation, ok: bool, took: Duration) {
        self.operation(operation, ok).observe(took);
    }

    pub fn layer_fetched(&self, bytes: u64, took: Duration) {
        self.layer_fetch_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.done(Operation::LayerFetch, true, took);
    }

    pub fn layer_cached(&self) {
        self.layers_cached.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn render(page: &mut Page, metrics: &ResourceMetrics, scrape: &Scrape<'_>) {
    page.metric(
        "nibrunner_storage_operation_seconds",
        "Something done to storage or the artifact store on an app's behalf, by what and how it ended.",
        "histogram",
    );
    for operation in OPERATIONS {
        for (index, outcome) in OUTCOMES.iter().enumerate() {
            page.histogram(
                "nibrunner_storage_operation_seconds",
                &[("operation", operation.as_str()), ("outcome", outcome)],
                metrics.operation(operation, index == 0),
            );
        }
    }

    page.metric(
        "nibrunner_layers_cached_total",
        "Layers a pass asked for that were already in the cache, so nothing was fetched.",
        "counter",
    );
    page.value(
        "nibrunner_layers_cached_total",
        &[],
        metrics.layers_cached.load(Ordering::Relaxed),
    );

    page.metric(
        "nibrunner_layer_fetch_bytes_total",
        "What has been pulled from the artifact store, as the store sized it.",
        "counter",
    );
    page.value(
        "nibrunner_layer_fetch_bytes_total",
        &[],
        metrics.layer_fetch_bytes.load(Ordering::Relaxed),
    );

    page.metric(
        "nibrunner_volume_state",
        "1 for the state a volume is in, 0 for every state it is not.",
        "gauge",
    );
    for volume in &scrape.report.volumes {
        for state in VOLUME_STATES {
            page.value(
                "nibrunner_volume_state",
                &[
                    ("volume", volume.volume_id.as_str()),
                    ("app", volume.app_id.as_str()),
                    ("state", volume_state_str(state)),
                ],
                u8::from(volume.state == state),
            );
        }
    }

    page.metric(
        "nibrunner_volume_size_bytes",
        "What a volume was set aside as.",
        "gauge",
    );
    for volume in &scrape.report.volumes {
        page.value(
            "nibrunner_volume_size_bytes",
            &[
                ("volume", volume.volume_id.as_str()),
                ("app", volume.app_id.as_str()),
            ],
            volume.size_bytes,
        );
    }

    page.metric(
        "nibrunner_volume_used_bytes",
        "What a guest reported filling of its volume, when it was last measured. Absent until it has been.",
        "gauge",
    );
    for volume in &scrape.report.volumes {
        if let Some(usage) = &volume.usage {
            page.value(
                "nibrunner_volume_used_bytes",
                &[
                    ("volume", volume.volume_id.as_str()),
                    ("app", volume.app_id.as_str()),
                ],
                usage.used_bytes,
            );
        }
    }

    page.metric(
        "nibrunner_checkpoint_state",
        "1 for the state a checkpoint is in, 0 for every state it is not.",
        "gauge",
    );
    for checkpoint in &scrape.report.checkpoints {
        for state in CHECKPOINT_STATES {
            page.value(
                "nibrunner_checkpoint_state",
                &[
                    ("checkpoint", checkpoint.checkpoint_id.as_str()),
                    ("volume", checkpoint.volume_id.as_str()),
                    ("state", checkpoint_state_str(state)),
                ],
                u8::from(checkpoint.state == state),
            );
        }
    }

    page.metric(
        "nibrunner_export_state",
        "1 for the state an export is in, 0 for every state it is not.",
        "gauge",
    );
    for export in &scrape.report.exports {
        for state in EXPORT_STATES {
            page.value(
                "nibrunner_export_state",
                &[
                    ("export", export.export_id.as_str()),
                    ("state", export_state_str(state)),
                ],
                u8::from(export.state == state),
            );
        }
    }

    page.metric(
        "nibrunner_export_size_bytes",
        "What an export came to, once written. Absent until it has been.",
        "gauge",
    );
    for export in &scrape.report.exports {
        if let Some(size_bytes) = export.size_bytes {
            page.value(
                "nibrunner_export_size_bytes",
                &[("export", export.export_id.as_str())],
                size_bytes,
            );
        }
    }

    page.metric(
        "nibrunner_slots",
        "Slots on this host: each holds an app's ports, tap and guest address, and an app with none is refused.",
        "gauge",
    );
    page.value("nibrunner_slots", &[("of", "used")], scrape.slots_used);
    page.value("nibrunner_slots", &[("of", "total")], nft_render::SLOT_COUNT);

    page.metric(
        "nibrunner_host_memory_available_bytes",
        "What the kernel says it could give out without swapping, as against the promises nibrunner_host_allocatable adds up. Absent where the kernel does not say.",
        "gauge",
    );
    if let Some(bytes) = scrape.memory_available_bytes {
        page.value("nibrunner_host_memory_available_bytes", &[], bytes);
    }

    page.metric(
        "nibrunner_instance_measured_timestamp_seconds",
        "When a guest last reported what it was using, as seconds since the epoch. 0 for one that never has; one that stopped is one this host can no longer hear.",
        "gauge",
    );
    for instance in &scrape.report.instances {
        page.value(
            "nibrunner_instance_measured_timestamp_seconds",
            &[("app", instance.app_id.as_str())],
            as_seconds(
                instance
                    .compute
                    .as_ref()
                    .map_or(0, |compute| compute.measured_at.epoch_ms().max(0) as u64),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::metrics::{render, HostMetrics};
    use crate::state::HostSnapshot;
    use crate::test_support::*;
    use protocol::{ComputeUsage, FilesystemUsage, ReportedCheckpoint, ReportedExport, Timestamp};

    fn lines_for<'a>(page: &'a str, name: &str) -> Vec<&'a str> {
        page.lines()
            .filter(|line| line.starts_with(&format!("{name}{{")) || line.starts_with(&format!("{name} ")))
            .collect()
    }

    #[test]
    fn what_the_host_holds_beyond_its_guests_is_on_the_page_with_what_each_cost() {
        let metrics = HostMetrics::default();
        metrics
            .resources
            .done(Operation::VolumeProvision, true, Duration::from_secs(3));
        metrics
            .resources
            .done(Operation::ExportWrite, false, Duration::from_secs(40));
        metrics.resources.layer_fetched(1_024, Duration::from_millis(300));
        metrics.resources.layer_cached();
        metrics.resources.layer_cached();

        let mut report = crate::domain::metrics::tests::report();
        report.volumes = vec![reported_volume(|volume| {
            volume.usage = Some(FilesystemUsage {
                total_bytes: 4_096,
                used_bytes: 512,
                measured_at: observed_at(),
            });
        })];
        report.checkpoints = vec![ReportedCheckpoint {
            checkpoint_id: checkpoint_id(),
            volume_id: volume_id(),
            state: protocol::CheckpointState::Ready,
            reference: None,
            ready_at: None,
            message: None,
        }];
        report.exports = vec![ReportedExport {
            export_id: export_id(),
            checkpoint_id: None,
            state: protocol::ExportState::Failed,
            size_bytes: None,
            ready_at: None,
            message: None,
        }];
        report.instances = vec![reported_instance(|instance| {
            instance.compute = Some(ComputeUsage {
                memory_total_bytes: 1,
                memory_used_bytes: 1,
                cpu_share: None,
                measured_at: Timestamp::from_epoch_ms(1_700_000_000_000),
            });
        })];
        let snapshot = HostSnapshot::default();
        let page = render(
            &metrics,
            &Scrape {
                report: &report,
                snapshot: &snapshot,
                now_ms: 0,
                slots_used: 7,
                memory_available_bytes: Some(1_000_000),
            },
        );

        assert!(page.contains(
            "nibrunner_storage_operation_seconds_count{operation=\"volume_provision\",outcome=\"ok\"} 1\n"
        ));
        assert!(page.contains(
            "nibrunner_storage_operation_seconds_count{operation=\"export_write\",outcome=\"failed\"} 1\n"
        ));
        assert!(page.contains(
            "nibrunner_storage_operation_seconds_count{operation=\"layer_fetch\",outcome=\"ok\"} 1\n"
        ));
        assert!(page.contains("nibrunner_layers_cached_total 2\n"));
        assert!(page.contains("nibrunner_layer_fetch_bytes_total 1024\n"));
        let states = lines_for(&page, "nibrunner_volume_state");
        assert_eq!(states.len(), VOLUME_STATES.len());
        assert!(states.contains(&"nibrunner_volume_state{volume=\"vol-1\",app=\"app-1\",state=\"ready\"} 1"));
        assert!(page.contains("nibrunner_volume_size_bytes{volume=\"vol-1\",app=\"app-1\"} 4096\n"));
        assert!(page.contains("nibrunner_volume_used_bytes{volume=\"vol-1\",app=\"app-1\"} 512\n"));
        assert!(page.contains(
            "nibrunner_checkpoint_state{checkpoint=\"chk-1\",volume=\"vol-1\",state=\"ready\"} 1\n"
        ));
        assert!(page.contains("nibrunner_export_state{export=\"exp-1\",state=\"failed\"} 1\n"));
        assert!(
            lines_for(&page, "nibrunner_export_size_bytes").is_empty(),
            "not written, no size"
        );
        assert!(page.contains("nibrunner_slots{of=\"used\"} 7\n"));
        assert!(page.contains(&format!(
            "nibrunner_slots{{of=\"total\"}} {}\n",
            nft_render::SLOT_COUNT
        )));
        assert!(page.contains("nibrunner_host_memory_available_bytes 1000000\n"));
        assert!(
            page.contains("nibrunner_instance_measured_timestamp_seconds{app=\"app-1\"} 1700000000.000\n")
        );
    }

    #[test]
    fn what_the_kernel_does_not_say_and_what_was_never_measured_are_left_out_and_zero_in_turn() {
        let metrics = HostMetrics::default();
        let mut report = crate::domain::metrics::tests::report();
        report.volumes = vec![reported_volume(|_| {})];
        report.instances = vec![reported_instance(|_| {})];
        let snapshot = HostSnapshot::default();
        let page = render(
            &metrics,
            &Scrape {
                report: &report,
                snapshot: &snapshot,
                now_ms: 0,
                slots_used: 0,
                memory_available_bytes: None,
            },
        );
        assert!(lines_for(&page, "nibrunner_host_memory_available_bytes").is_empty());
        assert!(lines_for(&page, "nibrunner_volume_used_bytes").is_empty());
        assert!(page.contains("nibrunner_instance_measured_timestamp_seconds{app=\"app-1\"} 0.000\n"));
    }
}
