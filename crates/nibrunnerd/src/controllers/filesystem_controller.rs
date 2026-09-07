use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::control_plane_controller::{CONTROL_PLANE_BACKOFF, IDLE_POLL_FLOOR};
use crate::controllers::Controller;
use crate::services::control_plane_service::ControlPlaneService;
use crate::services::filesystem_service::FilesystemService;

pub struct FilesystemController {
    control_plane: Arc<dyn ControlPlaneService>,
    filesystems: Arc<dyn FilesystemService>,
}

impl FilesystemController {
    pub fn new(
        control_plane: Arc<dyn ControlPlaneService>,
        filesystems: Arc<dyn FilesystemService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            control_plane,
            filesystems,
        })
    }

    pub async fn answer_once(&self, spent: Duration) -> Duration {
        let query = match self.control_plane.fetch_query().await {
            Ok(Some(query)) => query,
            Ok(None) => return IDLE_POLL_FLOOR.saturating_sub(spent),
            Err(error) => {
                self.control_plane.note(&error).await;
                tracing::warn!(error = %error.message(), "a read could not be collected");
                return CONTROL_PLANE_BACKOFF;
            }
        };
        let result = self.filesystems.answer(&query).await;
        if let Err(error) = self.control_plane.send_result(&result).await {
            self.control_plane.note(&error).await;
            tracing::warn!(error = %error.message(), "an answered read could not be handed back");
            return CONTROL_PLANE_BACKOFF;
        }
        tracing::info!(query_id = %query.query_id, app_id = %query.app_id, "a read was answered");
        Duration::ZERO
    }
}

#[async_trait]
impl Controller for FilesystemController {
    fn name(&self) -> &'static str {
        "filesystem"
    }

    async fn run(&self) {
        loop {
            let started = tokio::time::Instant::now();
            let wait = self.answer_once(started.elapsed()).await;
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::control_plane::ControlPlaneError;
    use crate::services::control_plane_service::MockControlPlaneService;
    use crate::services::filesystem_service::MockFilesystemService;
    use crate::test_support::app_id;

    fn unreachable() -> ControlPlaneError {
        ControlPlaneError::Unreachable {
            route: "/agent/filesystem-query".into(),
            reason: "connection refused".into(),
        }
    }

    fn query() -> protocol::FilesystemQuery {
        protocol::FilesystemQuery {
            query_id: protocol::FilesystemQueryId::parse("q-1").unwrap(),
            app_id: app_id(),
            path: protocol::GuestPath::parse("/").unwrap(),
        }
    }

    fn answered() -> Arc<MockFilesystemService> {
        let mut filesystems = MockFilesystemService::new();
        filesystems
            .expect_answer()
            .returning(|asked| protocol::FilesystemQueryResult {
                query_id: asked.query_id.clone(),
                outcome: protocol::FilesystemQueryOutcome::Failed {
                    message: "no microVM is running".into(),
                },
            });
        Arc::new(filesystems)
    }

    #[tokio::test]
    async fn a_read_is_fetched_answered_and_handed_back_before_the_next_is_asked_for() {
        let mut sequence = mockall::Sequence::new();
        let mut control_plane = MockControlPlaneService::new();
        control_plane
            .expect_fetch_query()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| Ok(Some(query())));
        control_plane
            .expect_send_result()
            .times(1)
            .in_sequence(&mut sequence)
            .withf(|result| result.query_id == query().query_id)
            .returning(|_| Ok(()));

        let controller = FilesystemController::new(Arc::new(control_plane), answered());
        assert_eq!(controller.answer_once(Duration::ZERO).await, Duration::ZERO);
    }

    #[tokio::test]
    async fn a_poll_that_was_held_open_has_already_spent_the_floor_it_would_have_waited() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_fetch_query().returning(|| Ok(None));
        let controller = FilesystemController::new(Arc::new(control_plane), answered());
        assert_eq!(controller.answer_once(IDLE_POLL_FLOOR).await, Duration::ZERO);
        assert_eq!(
            controller.answer_once(Duration::from_secs(1)).await,
            IDLE_POLL_FLOOR - Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn a_read_that_could_not_be_collected_backs_off_and_tells_the_session() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane
            .expect_fetch_query()
            .returning(|| Err(unreachable()));
        control_plane.expect_note().times(1).returning(|_| ());

        let controller = FilesystemController::new(Arc::new(control_plane), answered());
        assert_eq!(
            controller.answer_once(Duration::ZERO).await,
            CONTROL_PLANE_BACKOFF
        );
    }

    #[tokio::test]
    async fn a_guest_that_could_not_be_read_is_still_answered_rather_than_left_waiting() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_fetch_query().returning(|| Ok(Some(query())));
        control_plane
            .expect_send_result()
            .times(1)
            .withf(|result| matches!(result.outcome, protocol::FilesystemQueryOutcome::Failed { .. }))
            .returning(|_| Ok(()));

        let controller = FilesystemController::new(Arc::new(control_plane), answered());
        assert_eq!(controller.answer_once(Duration::ZERO).await, Duration::ZERO);
    }

    #[tokio::test]
    async fn an_answer_that_could_not_be_handed_back_backs_off_and_tells_the_session() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_fetch_query().returning(|| Ok(Some(query())));
        control_plane
            .expect_send_result()
            .returning(|_| Err(unreachable()));
        control_plane.expect_note().times(1).returning(|_| ());

        let controller = FilesystemController::new(Arc::new(control_plane), answered());
        assert_eq!(
            controller.answer_once(Duration::ZERO).await,
            CONTROL_PLANE_BACKOFF
        );
    }
}
