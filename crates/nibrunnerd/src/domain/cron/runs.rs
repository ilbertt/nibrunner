use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use protocol::{AppId, DeploymentId};
use tokio::sync::{watch, Notify};

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ActiveRuns {
    accepting: bool,
    next_id: u64,
    cancellations: BTreeMap<u64, watch::Sender<bool>>,
}

struct DeploymentRuns {
    deployment_id: DeploymentId,
    active: Mutex<ActiveRuns>,
    completed: Notify,
}

impl DeploymentRuns {
    fn new(deployment_id: DeploymentId) -> Self {
        Self {
            deployment_id,
            active: Mutex::new(ActiveRuns {
                accepting: true,
                next_id: 0,
                cancellations: BTreeMap::new(),
            }),
            completed: Notify::new(),
        }
    }

    fn start(self: &Arc<Self>) -> Option<CronRunLease> {
        let mut active = lock(&self.active);
        if !active.accepting {
            return None;
        }
        let id = active.next_id;
        active.next_id += 1;
        let (cancel, cancelled) = watch::channel(false);
        active.cancellations.insert(id, cancel);
        Some(CronRunLease {
            owner: self.clone(),
            id,
            cancelled,
        })
    }

    fn cancel(&self) {
        let mut active = lock(&self.active);
        active.accepting = false;
        for cancellation in active.cancellations.values() {
            cancellation.send_replace(true);
        }
    }

    async fn drain(&self) {
        loop {
            let completed = self.completed.notified();
            tokio::pin!(completed);
            completed.as_mut().enable();
            if lock(&self.active).cancellations.is_empty() {
                return;
            }
            completed.await;
        }
    }
}

#[derive(Default)]
pub struct CronRuns {
    owners: Mutex<BTreeMap<AppId, Arc<DeploymentRuns>>>,
    mutation: tokio::sync::Mutex<()>,
    stopping: AtomicBool,
}

impl CronRuns {
    pub fn start(&self, app_id: &AppId, deployment_id: &DeploymentId) -> Option<CronRunLease> {
        let owners = lock(&self.owners);
        if self.stopping.load(Ordering::Acquire) {
            return None;
        }
        owners
            .get(app_id)
            .filter(|owner| owner.deployment_id == *deployment_id)?
            .start()
    }

    pub async fn synchronize(&self, deployments: &[(AppId, DeploymentId)]) {
        let _mutation = self.mutation.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let retiring: Vec<_> = {
            let owners = lock(&self.owners);
            let retiring: Vec<_> = owners
                .iter()
                .filter(|(app_id, owner)| {
                    !lock(&owner.active).accepting
                        || !deployments.iter().any(|(wanted_app, wanted_deployment)| {
                            wanted_app == *app_id && wanted_deployment == &owner.deployment_id
                        })
                })
                .map(|(app_id, owner)| (app_id.clone(), owner.clone()))
                .collect();
            for (_, owner) in &retiring {
                owner.cancel();
            }
            retiring
        };
        for (_, owner) in &retiring {
            owner.drain().await;
        }
        let mut owners = lock(&self.owners);
        for (app_id, _) in retiring {
            owners.remove(&app_id);
        }
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        for (app_id, deployment_id) in deployments {
            owners
                .entry(app_id.clone())
                .or_insert_with(|| Arc::new(DeploymentRuns::new(deployment_id.clone())));
        }
    }

    pub async fn stop(&self, app_id: &AppId) {
        let _mutation = self.mutation.lock().await;
        let owner = {
            let owners = lock(&self.owners);
            let owner = owners.get(app_id).cloned();
            if let Some(owner) = &owner {
                owner.cancel();
            }
            owner
        };
        if let Some(owner) = owner {
            owner.drain().await;
        }
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let _mutation = self.mutation.lock().await;
        let owners: Vec<_> = {
            let owners = lock(&self.owners);
            owners.values().cloned().collect()
        };
        for owner in &owners {
            owner.cancel();
        }
        for owner in owners {
            owner.drain().await;
        }
    }
}

pub struct CronRunLease {
    owner: Arc<DeploymentRuns>,
    id: u64,
    cancelled: watch::Receiver<bool>,
}

impl CronRunLease {
    pub async fn cancelled(&mut self) {
        loop {
            if *self.cancelled.borrow_and_update() {
                return;
            }
            if self.cancelled.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Drop for CronRunLease {
    fn drop(&mut self) {
        lock(&self.owner.active).cancellations.remove(&self.id);
        self.owner.completed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_id, deployment_id};
    use std::time::Duration;

    #[tokio::test]
    async fn runs_may_overlap_and_the_same_deployment_keeps_them_owned() {
        let runs = CronRuns::default();
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        let mut first = runs.start(&app_id(), &deployment_id()).unwrap();
        let mut second = runs.start(&app_id(), &deployment_id()).unwrap();
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        assert!(tokio::time::timeout(Duration::from_millis(10), first.cancelled())
            .await
            .is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), second.cancelled())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_replacement_waits_for_old_runs_to_finish_before_accepting_new_runs() {
        let runs = Arc::new(CronRuns::default());
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        let mut first = runs.start(&app_id(), &deployment_id()).unwrap();
        let mut second = runs.start(&app_id(), &deployment_id()).unwrap();
        let next = DeploymentId::parse("dep-2").unwrap();
        let changing = tokio::spawn({
            let runs = runs.clone();
            let next = next.clone();
            async move { runs.synchronize(&[(app_id(), next)]).await }
        });
        first.cancelled().await;
        second.cancelled().await;
        assert!(runs.start(&app_id(), &deployment_id()).is_none());
        assert!(runs.start(&app_id(), &next).is_none());
        drop(first);
        assert!(!changing.is_finished());
        drop(second);
        changing.await.unwrap();
        assert!(runs.start(&app_id(), &next).is_some());
    }

    #[tokio::test]
    async fn suspending_an_app_cancels_and_drains_every_run_and_blocks_new_runs() {
        let runs = Arc::new(CronRuns::default());
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        let mut lease = runs.start(&app_id(), &deployment_id()).unwrap();
        let stopping = tokio::spawn({
            let runs = runs.clone();
            async move { runs.stop(&app_id()).await }
        });
        lease.cancelled().await;
        assert!(!stopping.is_finished());
        assert!(runs.start(&app_id(), &deployment_id()).is_none());
        drop(lease);
        stopping.await.unwrap();
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        assert!(runs.start(&app_id(), &deployment_id()).is_some());
    }

    #[tokio::test]
    async fn an_interrupted_replacement_keeps_old_runs_owned_until_they_are_drained() {
        let runs = Arc::new(CronRuns::default());
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        let mut lease = runs.start(&app_id(), &deployment_id()).unwrap();
        let next = DeploymentId::parse("dep-2").unwrap();
        let first = tokio::spawn({
            let runs = runs.clone();
            let next = next.clone();
            async move { runs.synchronize(&[(app_id(), next)]).await }
        });
        lease.cancelled().await;
        first.abort();
        let _ = first.await;
        let retry = tokio::spawn({
            let runs = runs.clone();
            let next = next.clone();
            async move { runs.synchronize(&[(app_id(), next)]).await }
        });
        tokio::task::yield_now().await;
        assert!(runs.start(&app_id(), &next).is_none());
        drop(lease);
        retry.await.unwrap();
        assert!(runs.start(&app_id(), &next).is_some());
    }

    #[tokio::test]
    async fn shutdown_drains_runs_and_later_synchronization_cannot_reopen_their_owners() {
        let runs = Arc::new(CronRuns::default());
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        let mut lease = runs.start(&app_id(), &deployment_id()).unwrap();
        let stopping = tokio::spawn({
            let runs = runs.clone();
            async move { runs.shutdown().await }
        });
        lease.cancelled().await;
        assert!(!stopping.is_finished());
        assert!(runs.start(&app_id(), &deployment_id()).is_none());
        drop(lease);
        stopping.await.unwrap();
        runs.synchronize(&[(app_id(), deployment_id())]).await;
        assert!(runs.start(&app_id(), &deployment_id()).is_none());
        runs.shutdown().await;
    }
}
