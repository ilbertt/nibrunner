//! Everything this daemon knows about the host it is on, in one place with one writer.
//!
//! A cache and never an authority: the microVM processes are what is running, the disk is what a
//! volume is, and this is what the last pass observed of them. A restarted daemon re-derives it
//! rather than trusting it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nft_render::AppTraffic;
use protocol::{AppId, ComputeUsage, FilesystemUsage, ReportedVolume, VolumeId};
use tokio::sync::{Notify, RwLock};

use crate::report::InstanceRecord;

#[derive(Debug, Default, Clone)]
pub struct HostSnapshot {
    pub records: BTreeMap<AppId, InstanceRecord>,
    /// Volumes this host removed, held until desired state stops naming them. A removal leaves
    /// nothing behind to observe, so this is the only thing that keeps saying it happened.
    pub deleted_volumes: BTreeMap<VolumeId, ReportedVolume>,
    pub volume_reports: Vec<ReportedVolume>,
    pub next_probe_at_ms: BTreeMap<AppId, i64>,
    /// The apps a snapshot is being taken of right now. Here rather than on the record because it
    /// may not outlive the daemon that set it: one that died mid-capture comes back with this
    /// empty, and the microVM it left behind reads as the crash it is rather than as a sleep
    /// nobody finished.
    pub snapshotting: BTreeSet<AppId>,
    /// The counters the last activity reading was taken from: the next reading is only meaningful
    /// against the one before it.
    pub app_traffic: BTreeMap<AppId, AppTraffic>,
    /// When each app was last reached by something that was not this host, which is what decides
    /// whether an `on-request` app may sleep.
    pub last_active_at_ms: BTreeMap<AppId, i64>,
    pub volume_usage: BTreeMap<AppId, FilesystemUsage>,
    pub compute_usage: BTreeMap<AppId, ComputeUsage>,
    pub converged: bool,
    /// Whether the last reconcile deferred something, and so whether re-running it would do
    /// anything.
    pub deferred_work: bool,
    /// Whether this host's isolation ruleset is in the kernel. A tenant is not started without it.
    pub isolated: bool,
}

pub type SharedState = Arc<HostState>;

pub struct HostState {
    snapshot: RwLock<HostSnapshot>,
    /// Says the host's own picture of what is running has moved, before the tick is up. The
    /// status loop chooses how long to sleep from the state as it stood when it last refreshed,
    /// so a microVM that comes up mid-sleep would otherwise wait out a second measured for a host
    /// where nothing was happening.
    refresh: Notify,
    /// Says a report is worth writing before the interval is up.
    report: Notify,
}

impl HostState {
    pub fn shared() -> SharedState {
        Arc::new(Self {
            snapshot: RwLock::new(HostSnapshot::default()),
            refresh: Notify::new(),
            report: Notify::new(),
        })
    }

    pub async fn snapshot(&self) -> HostSnapshot {
        self.snapshot.read().await.clone()
    }

    pub async fn records(&self) -> Vec<InstanceRecord> {
        self.snapshot.read().await.records.values().cloned().collect()
    }

    pub async fn record(&self, app_id: &AppId) -> Option<InstanceRecord> {
        self.snapshot.read().await.records.get(app_id).cloned()
    }

    pub async fn modify<T>(&self, change: impl FnOnce(&mut HostSnapshot) -> T) -> T {
        change(&mut *self.snapshot.write().await)
    }

    pub async fn put_record(&self, record: InstanceRecord) {
        self.snapshot.write().await.records.insert(record.app_id.clone(), record);
    }

    /// Merged into the record as it stands, never written over it. A probe is the longest thing a
    /// pass does, and a reconcile landing a start while one runs has already cleared the
    /// `stop_requested` the pass read: writing the whole record back would put that flag on again,
    /// and an instance carrying it is `stopping` for as long as its microVM is up — a state
    /// nothing forwards to and no later pass leaves. Merging is also what keeps an instance
    /// dropped mid-pass dropped.
    pub async fn update_record(&self, app_id: &AppId, change: impl FnOnce(&mut InstanceRecord)) {
        let mut snapshot = self.snapshot.write().await;
        if let Some(record) = snapshot.records.get_mut(app_id) {
            change(record);
        }
    }

    pub async fn drop_record(&self, app_id: &AppId) {
        self.snapshot.write().await.records.remove(app_id);
    }

    /// A request is evidence of use before any counter has seen it. The counters are read on
    /// their own cadence and a woken app's is new, which the activity reading treats as no
    /// evidence rather than as use — so without this a wake would leave the moment that had it
    /// sleeping.
    pub async fn mark_active(&self, app_id: &AppId, now_ms: i64) {
        self.snapshot.write().await.last_active_at_ms.insert(app_id.clone(), now_ms);
    }

    /// Taking a snapshot ends with the VMM gone, so while one is in flight a microVM that is not
    /// there is expected rather than lost. `stop_requested` cannot carry this: the refusal to
    /// sleep reads it as a stop already asked for and would refuse the very sleep it was marking.
    pub async fn mark_snapshotting(&self, app_id: &AppId, active: bool) {
        let mut snapshot = self.snapshot.write().await;
        if active {
            snapshot.snapshotting.insert(app_id.clone());
        } else {
            snapshot.snapshotting.remove(app_id);
        }
    }

    /// A microVM that has just come up is not left waiting on a delay measured for the one before
    /// it.
    pub async fn probe_at_once(&self, app_id: &AppId) {
        self.snapshot.write().await.next_probe_at_ms.remove(app_id);
    }

    pub async fn remember_deleted_volume(&self, report: ReportedVolume) {
        self.snapshot.write().await.deleted_volumes.insert(report.volume_id.clone(), report);
    }

    /// Once desired state stops naming it, the control plane has taken the removal in.
    pub async fn forget_deleted_volumes(&self, keep: &BTreeSet<VolumeId>) {
        self.snapshot.write().await.deleted_volumes.retain(|volume_id, _| keep.contains(volume_id));
    }

    pub fn signal_refresh(&self) {
        self.refresh.notify_one();
    }

    pub fn signal_report(&self) {
        self.report.notify_one();
    }

    pub async fn refresh_signalled(&self) {
        self.refresh.notified().await;
    }

    pub async fn report_signalled(&self) {
        self.report.notified().await;
    }
}

/// Rebuilt from what a reconcile found rather than added to, so what just happened to a volume
/// wins.
pub fn merge_volume_reports(existing: Vec<ReportedVolume>, updates: Vec<ReportedVolume>) -> Vec<ReportedVolume> {
    let mut merged: BTreeMap<VolumeId, ReportedVolume> =
        existing.into_iter().map(|report| (report.volume_id.clone(), report)).collect();
    for report in updates {
        merged.insert(report.volume_id.clone(), report);
    }
    merged.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_id, instance_record, volume_id};
    use protocol::{InstanceState, VolumeState};

    fn reported(state: VolumeState) -> ReportedVolume {
        ReportedVolume {
            volume_id: volume_id(),
            app_id: app_id(),
            state,
            size_bytes: 1,
            storage_prefix: None,
            device_path: None,
            usage: None,
            message: None,
        }
    }

    #[tokio::test]
    async fn a_record_is_merged_rather_than_written_over() {
        let state = HostState::shared();
        state.put_record(instance_record(|record| record.stop_requested = true)).await;
        state.update_record(&app_id(), |record| record.state = InstanceState::Starting).await;
        let record = state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Starting);
        assert!(record.stop_requested);
    }

    #[tokio::test]
    async fn an_instance_dropped_mid_pass_is_not_brought_back_by_a_write_that_lands_after() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        state.drop_record(&app_id()).await;
        state.update_record(&app_id(), |record| record.state = InstanceState::Running).await;
        assert!(state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn a_snapshot_mark_is_set_and_cleared_and_a_removal_is_remembered_until_taken_in() {
        let state = HostState::shared();
        state.mark_snapshotting(&app_id(), true).await;
        assert!(state.snapshot().await.snapshotting.contains(&app_id()));
        state.mark_snapshotting(&app_id(), false).await;
        assert!(state.snapshot().await.snapshotting.is_empty());

        state.remember_deleted_volume(reported(VolumeState::Deleted)).await;
        state.forget_deleted_volumes(&BTreeSet::from([volume_id()])).await;
        assert_eq!(state.snapshot().await.deleted_volumes.len(), 1);
        state.forget_deleted_volumes(&BTreeSet::new()).await;
        assert!(state.snapshot().await.deleted_volumes.is_empty());
    }

    #[test]
    fn what_just_happened_to_a_volume_wins_over_what_was_observed_of_it() {
        let merged = merge_volume_reports(vec![reported(VolumeState::Ready)], vec![reported(VolumeState::Deleted)]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].state, VolumeState::Deleted);
    }
}
