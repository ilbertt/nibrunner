use protocol::{
    CheckpointState, DesiredCheckpoint, HostDesiredState, ReportedCheckpoint, StateMessage, Timestamp,
};

use crate::domain::reconcile::plan::{CheckpointPlan, ObservedCheckpoint, ReconcilePlan};
use crate::host::Host;

pub async fn observe_checkpoints(host: &Host, desired: &HostDesiredState) -> Vec<ObservedCheckpoint> {
    let held = host.volumes.observe_checkpoints().await;
    desired
        .checkpoints
        .iter()
        .filter(|wanted| held.contains(&wanted.checkpoint_id))
        .map(|wanted| ObservedCheckpoint {
            checkpoint_id: wanted.checkpoint_id.clone(),
            volume_id: wanted.volume_id.clone(),
        })
        .collect()
}

pub async fn apply_checkpoints(host: &Host, plan: &ReconcilePlan) {
    let mut reports = Vec::new();
    for action in &plan.checkpoints {
        match action {
            CheckpointPlan::Create { desired } => reports.push(cut(host, desired).await),
            CheckpointPlan::Delete { desired } => release(host, desired).await,
            CheckpointPlan::None { checkpoint_id } => {
                if let Some(wanted) = plan_subject(host, checkpoint_id).await {
                    reports.push(ready(&wanted, None));
                }
            }
        }
    }
    host.state
        .modify(|snapshot| snapshot.checkpoint_reports = reports)
        .await;
}

async fn cut(host: &Host, desired: &DesiredCheckpoint) -> ReportedCheckpoint {
    match host.volumes.create_checkpoint(&desired.checkpoint_id).await {
        Ok(()) => {
            tracing::info!(
                checkpoint_id = %desired.checkpoint_id,
                volume_id = %desired.volume_id,
                "checkpoint cut"
            );
            ready(desired, Some(Timestamp::now()))
        }
        Err(error) => ReportedCheckpoint {
            checkpoint_id: desired.checkpoint_id.clone(),
            volume_id: desired.volume_id.clone(),
            state: CheckpointState::Failed,
            reference: None,
            ready_at: None,
            message: Some(StateMessage::new(error.message())),
        },
    }
}

async fn release(host: &Host, desired: &DesiredCheckpoint) {
    if let Err(error) = host.volumes.delete_checkpoint(&desired.checkpoint_id).await {
        tracing::error!(
            checkpoint_id = %desired.checkpoint_id,
            error = %error.message(),
            "checkpoint not deleted; storage reclamation stays paused"
        );
    }
}

fn ready(desired: &DesiredCheckpoint, ready_at: Option<Timestamp>) -> ReportedCheckpoint {
    ReportedCheckpoint {
        checkpoint_id: desired.checkpoint_id.clone(),
        volume_id: desired.volume_id.clone(),
        state: CheckpointState::Ready,
        reference: Some(StateMessage::new(desired.checkpoint_id.to_string())),
        ready_at,
        message: None,
    }
}

async fn plan_subject(host: &Host, checkpoint_id: &protocol::CheckpointId) -> Option<DesiredCheckpoint> {
    host.cache
        .lock()
        .await
        .latest()?
        .checkpoints
        .iter()
        .find(|wanted| &wanted.checkpoint_id == checkpoint_id)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::reconcile::plan::plan_reconcile;
    use crate::test_support::*;
    use protocol::{CheckpointId, DesiredPresence, VolumeId};

    fn wanted(state: DesiredPresence) -> DesiredCheckpoint {
        DesiredCheckpoint {
            checkpoint_id: checkpoint_id(),
            volume_id: VolumeId::parse("vol-1").unwrap(),
            desired_state: state,
        }
    }

    #[tokio::test]
    async fn a_backend_that_cannot_pin_a_view_reports_the_checkpoint_failed() {
        let host = test_host().await;
        let desired = desired_state(|state| state.checkpoints = vec![wanted(DesiredPresence::Present)]);
        host.cache.lock().await.accept(desired.clone());
        let plan = plan_reconcile(&desired, &observed_state(|_| {}));

        apply_checkpoints(host.arc(), &plan).await;

        let reports = host.state.snapshot().await.checkpoint_reports;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, CheckpointState::Failed);
        assert!(reports[0]
            .message
            .as_ref()
            .unwrap()
            .as_str()
            .contains("cannot be checkpointed"));
    }

    #[tokio::test]
    async fn only_the_checkpoints_the_document_names_are_reported() {
        let host = test_host().await;
        let desired = desired_state(|state| state.checkpoints = vec![wanted(DesiredPresence::Present)]);
        assert!(observe_checkpoints(host.arc(), &desired).await.is_empty());
    }

    async fn host_over(volumes: crate::adapters::volumes::MockVolumeBackend) -> TestHost {
        let mut host = test_host().await;
        std::sync::Arc::get_mut(&mut host.host)
            .expect("nothing else holds this host yet")
            .volumes = std::sync::Arc::new(volumes);
        host
    }

    fn holding(checkpoints: Vec<CheckpointId>) -> crate::adapters::volumes::MockVolumeBackend {
        let mut volumes = crate::adapters::volumes::MockVolumeBackend::new();
        volumes
            .expect_observe_checkpoints()
            .returning(move || checkpoints.clone());
        volumes
    }

    async fn planned(host: &crate::host::Host, desired: &protocol::HostDesiredState) -> ReconcilePlan {
        host.cache.lock().await.accept(desired.clone());
        let observed = observe_checkpoints(host, desired).await;
        plan_reconcile(desired, &observed_state(|state| state.checkpoints = observed))
    }

    #[tokio::test]
    async fn a_checkpoint_the_document_asks_for_is_cut_and_reported_ready() {
        let mut volumes = holding(vec![]);
        volumes
            .expect_create_checkpoint()
            .times(1)
            .withf(|held| held.as_str() == "chk-1")
            .returning(|_| Ok(()));
        let host = host_over(volumes).await;
        let desired = desired_state(|state| state.checkpoints = vec![wanted(DesiredPresence::Present)]);
        let plan = planned(host.arc(), &desired).await;

        apply_checkpoints(host.arc(), &plan).await;

        let reports = host.state.snapshot().await.checkpoint_reports;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, CheckpointState::Ready);
        assert_eq!(reports[0].volume_id, volume_id());
        assert!(reports[0].ready_at.is_some());
        assert_eq!(reports[0].reference.as_ref().unwrap().as_str(), "chk-1");
    }

    #[tokio::test]
    async fn one_this_host_already_cut_is_reported_from_the_document_rather_than_cut_again() {
        let mut volumes = holding(vec![checkpoint_id()]);
        volumes.expect_create_checkpoint().never();
        let host = host_over(volumes).await;
        let desired = desired_state(|state| state.checkpoints = vec![wanted(DesiredPresence::Present)]);
        let plan = planned(host.arc(), &desired).await;

        apply_checkpoints(host.arc(), &plan).await;

        let reports = host.state.snapshot().await.checkpoint_reports;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, CheckpointState::Ready);
        assert_eq!(reports[0].ready_at, None);
    }

    #[tokio::test]
    async fn one_the_document_no_longer_wants_is_released_and_stops_being_reported() {
        let mut volumes = holding(vec![checkpoint_id()]);
        volumes
            .expect_delete_checkpoint()
            .times(1)
            .withf(|held| held.as_str() == "chk-1")
            .returning(|_| Ok(()));
        let host = host_over(volumes).await;
        let desired = desired_state(|state| state.checkpoints = vec![wanted(DesiredPresence::Absent)]);
        let plan = planned(host.arc(), &desired).await;

        apply_checkpoints(host.arc(), &plan).await;

        assert!(host.state.snapshot().await.checkpoint_reports.is_empty());
    }

    #[tokio::test]
    async fn a_release_the_backend_refused_leaves_the_pass_running_rather_than_raising() {
        let mut volumes = holding(vec![checkpoint_id()]);
        volumes.expect_delete_checkpoint().returning(|_| {
            Err(crate::adapters::volumes::VolumeError::NoCheckpoints {
                what: "a volume kept as a file on this host's own disk",
            })
        });
        let host = host_over(volumes).await;
        let desired = desired_state(|state| state.checkpoints = vec![wanted(DesiredPresence::Absent)]);
        let plan = planned(host.arc(), &desired).await;

        apply_checkpoints(host.arc(), &plan).await;

        assert!(host.state.snapshot().await.checkpoint_reports.is_empty());
    }

    #[tokio::test]
    async fn a_checkpoint_this_host_was_never_handed_a_document_for_is_reported_on_by_nobody() {
        let host = test_host().await;
        let plan = ReconcilePlan {
            checkpoints: vec![CheckpointPlan::None {
                checkpoint_id: checkpoint_id(),
            }],
            ..Default::default()
        };

        apply_checkpoints(host.arc(), &plan).await;

        assert!(host.state.snapshot().await.checkpoint_reports.is_empty());
    }

    #[tokio::test]
    async fn a_checkpoint_the_host_holds_that_the_document_does_not_name_is_not_observed() {
        let host = host_over(holding(vec![
            checkpoint_id(),
            CheckpointId::parse("chk-9").unwrap(),
        ]))
        .await;
        let desired = desired_state(|state| state.checkpoints = vec![wanted(DesiredPresence::Present)]);

        let observed = observe_checkpoints(host.arc(), &desired).await;

        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].checkpoint_id, checkpoint_id());
        assert_eq!(observed[0].volume_id, volume_id());
    }
}
