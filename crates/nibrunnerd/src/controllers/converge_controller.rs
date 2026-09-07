use std::sync::Arc;

use async_trait::async_trait;

use crate::controllers::Controller;
use crate::desired::DesiredStateWatch;
use crate::host::Host;
use crate::services::reconcile_service::ReconcileService;

pub struct ConvergeController {
    host: Arc<Host>,
    reconciler: Arc<dyn ReconcileService>,
}

impl ConvergeController {
    pub fn new(host: Arc<Host>, reconciler: Arc<dyn ReconcileService>) -> Arc<Self> {
        Arc::new(Self { host, reconciler })
    }

    pub async fn converge_cached(&self) -> bool {
        let Some(cached) = self.host.cached_desired_state().await else {
            return false;
        };
        if !self.host.cache.lock().await.accept(cached.clone()) {
            return false;
        }
        self.reconciler.reconcile(&cached).await;
        true
    }

    pub async fn converge_once(&self) -> bool {
        match crate::desired::read_desired_state(&self.host.config.desired_state_file) {
            Ok(Some(desired)) => {
                let news = self.host.cache.lock().await.accept(desired.clone());
                if news {
                    let _ = crate::desired::cache_desired_state(
                        &self.host.config.cached_desired_state_file(),
                        &desired,
                    );
                } else if !self.host.state.snapshot().await.deferred_work {
                    return false;
                }
                self.reconciler.reconcile(&desired).await;
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::error!(error = %error.message(), "the desired state file was not read");
                false
            }
        }
    }
}

#[async_trait]
impl Controller for ConvergeController {
    fn name(&self) -> &'static str {
        "converge"
    }

    async fn run(&self) {
        let watch = DesiredStateWatch::on(&self.host.config.desired_state_file);
        self.converge_cached().await;
        loop {
            self.converge_once().await;
            watch.changed().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::reconcile_service::MockReconcileService;
    use crate::test_support::*;

    fn controller(host: &TestHost, reconciler: MockReconcileService) -> Arc<ConvergeController> {
        ConvergeController::new(host.arc().clone(), Arc::new(reconciler))
    }

    #[tokio::test]
    async fn a_document_that_appeared_is_converged_on() {
        let host = test_host().await;
        let desired = desired_state(|state| state.instances = vec![desired_instance(|_| {})]);
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired).unwrap();

        let mut reconciler = MockReconcileService::new();
        let wanted = desired.clone();
        reconciler
            .expect_reconcile()
            .times(1)
            .withf(move |seen| *seen == wanted)
            .returning(|_| ());

        assert!(controller(&host, reconciler).converge_once().await);
    }

    #[tokio::test]
    async fn a_missing_document_is_the_ordinary_state_of_a_fresh_host() {
        let host = test_host().await;
        let mut reconciler = MockReconcileService::new();
        reconciler.expect_reconcile().never();
        assert!(!controller(&host, reconciler).converge_once().await);
    }

    #[tokio::test]
    async fn a_document_that_has_not_moved_does_not_run_a_second_pass() {
        let host = test_host().await;
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired_state(|_| {})).unwrap();

        let mut reconciler = MockReconcileService::new();
        reconciler.expect_reconcile().times(1).returning(|_| ());
        let controller = controller(&host, reconciler);
        assert!(controller.converge_once().await);
        assert!(!controller.converge_once().await);
    }

    #[tokio::test]
    async fn work_the_last_pass_deferred_is_carried_even_though_the_document_stood_still() {
        let host = test_host().await;
        crate::desired::cache_desired_state(&host.config.desired_state_file, &desired_state(|_| {})).unwrap();

        let mut reconciler = MockReconcileService::new();
        reconciler.expect_reconcile().times(2).returning(|_| ());
        let controller = controller(&host, reconciler);
        assert!(controller.converge_once().await);
        host.state.modify(|snapshot| snapshot.deferred_work = true).await;
        assert!(controller.converge_once().await);
    }

    #[tokio::test]
    async fn a_document_that_cannot_be_read_is_logged_rather_than_converged_on() {
        let host = test_host().await;
        crate::json_store::make_directory(host.config.desired_state_file.parent().unwrap(), 0o700).unwrap();
        std::fs::write(&host.config.desired_state_file, b"not json at all").unwrap();

        let mut reconciler = MockReconcileService::new();
        reconciler.expect_reconcile().never();
        assert!(!controller(&host, reconciler).converge_once().await);
    }

    #[tokio::test]
    async fn the_last_document_this_host_was_given_is_what_a_restart_converges_on_first() {
        let host = test_host().await;
        crate::desired::cache_desired_state(&host.config.cached_desired_state_file(), &desired_state(|_| {}))
            .unwrap();

        let mut reconciler = MockReconcileService::new();
        reconciler.expect_reconcile().times(1).returning(|_| ());
        assert!(controller(&host, reconciler).converge_cached().await);
    }

    #[tokio::test]
    async fn a_host_that_was_never_given_a_document_has_nothing_cached_to_converge_on() {
        let host = test_host().await;
        let mut reconciler = MockReconcileService::new();
        reconciler.expect_reconcile().never();
        assert!(!controller(&host, reconciler).converge_cached().await);
    }
}
