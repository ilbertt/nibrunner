use std::collections::{BTreeMap, BTreeSet};

use protocol::{AppId, HostDesiredState, ReportedVolume, StateMessage, VolumeId, VolumeState};

use crate::host::Host;
use crate::services::reconcile::plan::{ObservedState, ObservedVolume, ReconcilePlan, VolumePlan};
use crate::services::report::InstanceRecord;
use crate::state::merge_volume_reports;

pub fn volume_owners(
    desired: &HostDesiredState,
    records: &BTreeMap<AppId, InstanceRecord>,
) -> BTreeMap<VolumeId, AppId> {
    let mut owners: BTreeMap<VolumeId, AppId> = records
        .values()
        .map(|record| (record.volume_id.clone(), record.app_id.clone()))
        .collect();
    for volume in &desired.volumes {
        owners.insert(volume.volume_id.clone(), volume.app_id.clone());
    }
    owners
}

pub async fn observe_volumes(host: &Host, owners: &BTreeMap<VolumeId, AppId>) -> Vec<ObservedVolume> {
    host.volumes
        .observe(owners)
        .await
        .into_iter()
        .filter_map(|backing| {
            let app_id = owners.get(&backing.volume_id)?.clone();
            Some(ObservedVolume {
                volume_id: backing.volume_id,
                app_id,
                attached: backing.attached,
                size_bytes: backing.size_bytes,
                storage_prefix: backing.storage_prefix,
                device_path: backing.device_path,
            })
        })
        .collect()
}

pub fn to_reported_volume(observed: &ObservedVolume) -> ReportedVolume {
    ReportedVolume {
        volume_id: observed.volume_id.clone(),
        app_id: observed.app_id.clone(),
        state: if observed.attached {
            VolumeState::Ready
        } else {
            VolumeState::Detached
        },
        size_bytes: observed.size_bytes,
        storage_prefix: Some(observed.storage_prefix.clone()),
        device_path: observed.device_path.clone(),
        usage: None,
        message: None,
    }
}

pub async fn apply_volumes(
    host: &Host,
    plan: &ReconcilePlan,
    observed: &ObservedState,
    desired: &HostDesiredState,
) {
    let mut updates = Vec::new();
    for action in &plan.volumes {
        match action {
            VolumePlan::Provision { desired } => {
                let report = match host.volumes.provision(desired).await {
                    Ok(attached) => ReportedVolume {
                        volume_id: attached.volume_id,
                        app_id: desired.app_id.clone(),
                        state: VolumeState::Ready,
                        size_bytes: attached.size_bytes,
                        storage_prefix: Some(attached.storage_prefix),
                        device_path: Some(attached.device_path),
                        usage: None,
                        message: None,
                    },
                    Err(error) => {
                        tracing::error!(volume_id = %desired.volume_id, error = %error.message(), "volume provisioning failed");
                        ReportedVolume {
                            volume_id: desired.volume_id.clone(),
                            app_id: desired.app_id.clone(),
                            state: VolumeState::Failed,
                            size_bytes: desired.size_bytes,
                            storage_prefix: None,
                            device_path: None,
                            usage: None,
                            message: Some(StateMessage::new(error.message())),
                        }
                    }
                };
                updates.push(report);
            }
            VolumePlan::Blocked { desired, blocked_by } => {
                tracing::warn!(
                    volume_id = %desired.volume_id,
                    blocked_by = ?blocked_by.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "volume removal deferred: still held"
                );
            }
            _ => {}
        }
    }

    let still_named: BTreeSet<VolumeId> = desired
        .volumes
        .iter()
        .map(|volume| volume.volume_id.clone())
        .collect();
    host.state.forget_deleted_volumes(&still_named).await;

    let existing: Vec<ReportedVolume> = observed.volumes.iter().map(to_reported_volume).collect();
    let snapshot = host.state.snapshot().await;
    let mut all_updates: Vec<ReportedVolume> = snapshot.deleted_volumes.values().cloned().collect();
    all_updates.extend(updates);
    let merged = merge_volume_reports(existing, all_updates);
    host.state
        .modify(|snapshot| snapshot.volume_reports = merged)
        .await;
}

pub async fn apply_teardowns(host: &Host, plan: &ReconcilePlan) {
    for action in &plan.volumes {
        let VolumePlan::Teardown { desired } = action else {
            continue;
        };
        match host.volumes.teardown(&desired.volume_id, &desired.app_id).await {
            Ok(()) => {
                host.allocator.lock().await.release(&desired.app_id);
                let report = ReportedVolume {
                    volume_id: desired.volume_id.clone(),
                    app_id: desired.app_id.clone(),
                    state: VolumeState::Deleted,
                    size_bytes: 0,
                    storage_prefix: None,
                    device_path: None,
                    usage: None,
                    message: None,
                };
                host.state.remember_deleted_volume(report.clone()).await;
                host.state
                    .modify(|snapshot| {
                        snapshot.volume_reports =
                            merge_volume_reports(std::mem::take(&mut snapshot.volume_reports), vec![report]);
                    })
                    .await;
            }
            Err(error) => {
                tracing::error!(volume_id = %desired.volume_id, error = %error.message(), "volume teardown failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn a_volume_being_deleted_is_still_owned_once_its_record_is_gone() {
        let desired = desired_state(|state| state.volumes = vec![desired_volume(|_| {})]);
        let owners = volume_owners(&desired, &BTreeMap::new());
        assert_eq!(owners.get(&volume_id()), Some(&app_id()));

        let records = BTreeMap::from([(app_id(), instance_record(|_| {}))]);
        let held = volume_owners(&desired_state(|_| {}), &records);
        assert_eq!(held.get(&volume_id()), Some(&app_id()));
    }

    #[tokio::test]
    async fn a_backing_with_no_app_is_left_out_rather_than_reported_under_a_guess() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        assert!(observe_volumes(&host, &BTreeMap::new()).await.is_empty());
        let owners = BTreeMap::from([(volume_id(), app_id())]);
        let observed = observe_volumes(&host, &owners).await;
        assert_eq!(observed.len(), 1);
        assert_eq!(to_reported_volume(&observed[0]).state, VolumeState::Ready);
    }

    #[tokio::test]
    async fn a_teardown_gives_the_slot_back_and_says_the_volume_is_gone() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        let plan = ReconcilePlan {
            volumes: vec![VolumePlan::Teardown {
                desired: desired_volume(|volume| volume.desired_state = protocol::DesiredPresence::Absent),
            }],
            ..Default::default()
        };
        apply_teardowns(&host, &plan).await;
        assert!(host.slot_of(&app_id()).await.is_none());
        let snapshot = host.state.snapshot().await;
        assert_eq!(snapshot.volume_reports[0].state, VolumeState::Deleted);
        assert!(snapshot.deleted_volumes.contains_key(&volume_id()));
        assert!(host.volumes.observe(&Default::default()).await.is_empty());
    }

    #[tokio::test]
    async fn a_volume_that_could_not_be_provisioned_is_reported_failed_with_the_reason() {
        let host = test_host().await;
        host.volumes
            .provision(&desired_volume(|volume| {
                volume.size_bytes = VOLUME_SIZE_BYTES * 4
            }))
            .await
            .unwrap();
        let plan = ReconcilePlan {
            volumes: vec![VolumePlan::Provision {
                desired: desired_volume(|_| {}),
            }],
            ..Default::default()
        };
        apply_volumes(&host, &plan, &ObservedState::default(), &desired_state(|_| {})).await;
        let reports = host.state.snapshot().await.volume_reports;
        assert_eq!(reports[0].state, VolumeState::Failed);
        assert!(reports[0]
            .message
            .as_ref()
            .unwrap()
            .as_str()
            .contains("cannot be resized down"));
    }
}
