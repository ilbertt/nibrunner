use protocol::{
    CheckpointState, DesiredCheckpoint, HostDesiredState, ReportedCheckpoint, StateMessage, Timestamp,
};

use crate::host::Host;
use crate::services::reconcile::plan::{CheckpointPlan, ObservedCheckpoint, ReconcilePlan};

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
    use crate::services::reconcile::plan::plan_reconcile;
    use crate::test_support::*;
    use protocol::{DesiredPresence, VolumeId};

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
}
