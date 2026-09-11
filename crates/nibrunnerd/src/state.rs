use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nft_render::AppTraffic;
use protocol::{AppId, ComputeUsage, FilesystemUsage, ReportedVolume, UsageMeters, VolumeId};
use tokio::sync::{Notify, RwLock};

use crate::domain::metrics::converge::Deploy;
use crate::domain::report::InstanceRecord;

#[derive(Debug, Default, Clone)]
pub struct HostSnapshot {
    pub records: BTreeMap<AppId, InstanceRecord>,
    // Only ever this daemon's own account of what it set out on: a restart starts each over,
    // and so is measured as one.
    pub deploys: BTreeMap<AppId, Deploy>,
    pub deleted_volumes: BTreeMap<VolumeId, ReportedVolume>,
    pub volume_reports: Vec<ReportedVolume>,
    pub checkpoint_reports: Vec<protocol::ReportedCheckpoint>,
    pub export_reports: Vec<protocol::ReportedExport>,
    pub next_probe_at_ms: BTreeMap<AppId, i64>,
    pub snapshotting: BTreeSet<AppId>,
    pub app_traffic: BTreeMap<AppId, AppTraffic>,
    pub last_active_at_ms: BTreeMap<AppId, i64>,
    pub volume_usage: BTreeMap<AppId, FilesystemUsage>,
    pub compute_usage: BTreeMap<AppId, ComputeUsage>,
    pub compute_ticks: BTreeMap<AppId, guest_contract::filesystem::MeasuredCompute>,
    pub meters: BTreeMap<AppId, UsageMeters>,
    // Only ever this daemon's own note of when it last metered, so a restart credits nothing
    // for the stretch it was not there to watch.
    pub metered_at_ms: Option<i64>,
    pub converged: bool,
    pub deferred_work: bool,
    pub isolated: bool,
}

pub type SharedState = Arc<HostState>;

pub struct HostState {
    snapshot: RwLock<HostSnapshot>,
    refresh: Notify,
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
        self.snapshot
            .write()
            .await
            .records
            .insert(record.app_id.clone(), record);
    }

    pub async fn update_record(&self, app_id: &AppId, change: impl FnOnce(&mut InstanceRecord)) {
        let mut snapshot = self.snapshot.write().await;
        if let Some(record) = snapshot.records.get_mut(app_id) {
            change(record);
        }
    }

    pub async fn drop_record(&self, app_id: &AppId) {
        self.snapshot.write().await.records.remove(app_id);
    }

    pub async fn mark_active(&self, app_id: &AppId, now_ms: i64) {
        self.snapshot
            .write()
            .await
            .last_active_at_ms
            .insert(app_id.clone(), now_ms);
    }

    pub async fn mark_snapshotting(&self, app_id: &AppId, active: bool) {
        let mut snapshot = self.snapshot.write().await;
        if active {
            snapshot.snapshotting.insert(app_id.clone());
        } else {
            snapshot.snapshotting.remove(app_id);
        }
    }

    pub async fn probe_at_once(&self, app_id: &AppId) {
        self.snapshot.write().await.next_probe_at_ms.remove(app_id);
    }

    pub async fn remember_deleted_volume(&self, report: ReportedVolume) {
        self.snapshot
            .write()
            .await
            .deleted_volumes
            .insert(report.volume_id.clone(), report);
    }

    pub async fn forget_deleted_volumes(&self, keep: &BTreeSet<VolumeId>) {
        self.snapshot
            .write()
            .await
            .deleted_volumes
            .retain(|volume_id, _| keep.contains(volume_id));
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

pub fn merge_volume_reports(
    existing: Vec<ReportedVolume>,
    updates: Vec<ReportedVolume>,
) -> Vec<ReportedVolume> {
    let mut merged: BTreeMap<VolumeId, ReportedVolume> = existing
        .into_iter()
        .map(|report| (report.volume_id.clone(), report))
        .collect();
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
        state
            .put_record(instance_record(|record| record.stop_requested = true))
            .await;
        state
            .update_record(&app_id(), |record| record.state = InstanceState::Starting)
            .await;
        let record = state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Starting);
        assert!(record.stop_requested);
    }

    #[tokio::test]
    async fn an_instance_dropped_mid_pass_is_not_brought_back_by_a_write_that_lands_after() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        state.drop_record(&app_id()).await;
        state
            .update_record(&app_id(), |record| record.state = InstanceState::Running)
            .await;
        assert!(state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn a_snapshot_mark_is_set_and_cleared_and_a_removal_is_remembered_until_taken_in() {
        let state = HostState::shared();
        state.mark_snapshotting(&app_id(), true).await;
        assert!(state.snapshot().await.snapshotting.contains(&app_id()));
        state.mark_snapshotting(&app_id(), false).await;
        assert!(state.snapshot().await.snapshotting.is_empty());

        state
            .remember_deleted_volume(reported(VolumeState::Deleted))
            .await;
        state.forget_deleted_volumes(&BTreeSet::from([volume_id()])).await;
        assert_eq!(state.snapshot().await.deleted_volumes.len(), 1);
        state.forget_deleted_volumes(&BTreeSet::new()).await;
        assert!(state.snapshot().await.deleted_volumes.is_empty());
    }

    #[test]
    fn what_just_happened_to_a_volume_wins_over_what_was_observed_of_it() {
        let merged = merge_volume_reports(
            vec![reported(VolumeState::Ready)],
            vec![reported(VolumeState::Deleted)],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].state, VolumeState::Deleted);
    }

    #[test]
    fn a_volume_only_one_side_knows_of_is_kept_rather_than_dropped_by_the_merge() {
        let elsewhere = ReportedVolume {
            volume_id: VolumeId::parse("vol-2").unwrap(),
            ..reported(VolumeState::Ready)
        };
        let merged = merge_volume_reports(vec![reported(VolumeState::Ready)], vec![elsewhere.clone()]);
        assert_eq!(merged.len(), 2);
        assert!(merged
            .iter()
            .any(|report| report.volume_id == elsewhere.volume_id));
        assert!(merge_volume_reports(vec![], vec![]).is_empty());
    }

    #[tokio::test]
    async fn a_host_that_has_done_nothing_yet_holds_nothing_and_claims_nothing() {
        let state = HostState::shared();
        let snapshot = state.snapshot().await;
        assert!(snapshot.records.is_empty());
        assert!(!snapshot.converged);
        assert!(!snapshot.isolated);
        assert!(!snapshot.deferred_work);
        assert!(state.records().await.is_empty());
        assert!(state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn every_record_the_host_holds_is_handed_back_in_one_reading() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        state
            .put_record(instance_record(|record| {
                record.app_id = AppId::parse("app-2").unwrap()
            }))
            .await;
        let records = state.records().await;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].app_id, app_id());

        state
            .put_record(instance_record(|record| record.state = InstanceState::Failed))
            .await;
        assert_eq!(state.records().await.len(), 2);
        assert_eq!(
            state.record(&app_id()).await.unwrap().state,
            InstanceState::Failed
        );
    }

    #[tokio::test]
    async fn a_change_made_under_one_lock_is_seen_whole_and_hands_back_what_it_decided() {
        let state = HostState::shared();
        let decided = state
            .modify(|snapshot| {
                snapshot.converged = true;
                snapshot.isolated = true;
                snapshot.records.len()
            })
            .await;
        assert_eq!(decided, 0);
        let snapshot = state.snapshot().await;
        assert!(snapshot.converged);
        assert!(snapshot.isolated);
    }

    #[tokio::test]
    async fn a_request_that_reached_an_app_moves_its_last_activity_forward() {
        let state = HostState::shared();
        state.mark_active(&app_id(), 1_000).await;
        assert_eq!(
            state.snapshot().await.last_active_at_ms.get(&app_id()),
            Some(&1_000)
        );
        state.mark_active(&app_id(), 2_000).await;
        assert_eq!(
            state.snapshot().await.last_active_at_ms.get(&app_id()),
            Some(&2_000)
        );
    }

    #[tokio::test]
    async fn asking_for_a_probe_at_once_forgets_when_the_next_one_was_due() {
        let state = HostState::shared();
        state
            .modify(|snapshot| snapshot.next_probe_at_ms.insert(app_id(), 9_999))
            .await;
        state.probe_at_once(&app_id()).await;
        assert!(state.snapshot().await.next_probe_at_ms.is_empty());
        state.probe_at_once(&app_id()).await;
    }

    #[tokio::test]
    async fn a_signal_raised_before_anybody_waited_still_wakes_the_next_waiter() {
        let state = HostState::shared();
        state.signal_refresh();
        state.signal_report();
        let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            state.refresh_signalled().await;
            state.report_signalled().await;
        })
        .await;
        assert!(waited.is_ok(), "a signal raised first was lost");
    }

    #[tokio::test]
    async fn a_loop_waiting_on_a_signal_is_woken_by_the_pass_that_raises_it() {
        let state = HostState::shared();
        let waiting = tokio::spawn({
            let state = state.clone();
            async move { state.refresh_signalled().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        state.signal_refresh();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .is_ok());
    }
}
