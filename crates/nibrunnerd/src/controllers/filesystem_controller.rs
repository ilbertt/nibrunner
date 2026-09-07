use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::control_plane_controller::{CONTROL_PLANE_BACKOFF, IDLE_POLL_FLOOR};
use crate::controllers::Controller;
use crate::services::control_plane_service::ControlPlaneService;

pub struct FilesystemController {
    control_plane: Arc<dyn ControlPlaneService>,
}

impl FilesystemController {
    pub fn new(control_plane: Arc<dyn ControlPlaneService>) -> Arc<Self> {
        Arc::new(Self { control_plane })
    }

    pub async fn answer_once(&self, spent: Duration) -> Duration {
        match self.control_plane.answer_one_query().await {
            Ok(Some(query)) => {
                tracing::info!(query_id = %query.query_id, app_id = %query.app_id, "a read was answered");
                Duration::ZERO
            }
            Ok(None) => IDLE_POLL_FLOOR.saturating_sub(spent),
            Err(error) => {
                self.control_plane.note(&error).await;
                tracing::warn!(error = %error.message(), "a read could not be collected");
                CONTROL_PLANE_BACKOFF
            }
        }
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
    use crate::test_support::app_id;

    fn unreachable() -> ControlPlaneError {
        ControlPlaneError::Unreachable {
            route: "/agent/filesystem-query".into(),
            reason: "connection refused".into(),
        }
    }

    #[tokio::test]
    async fn a_read_that_was_answered_is_followed_straight_away_by_asking_for_the_next() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_answer_one_query().returning(|| {
            Ok(Some(protocol::FilesystemQuery {
                query_id: protocol::FilesystemQueryId::parse("q-1").unwrap(),
                app_id: app_id(),
                path: protocol::GuestPath::parse("/").unwrap(),
            }))
        });
        let controller = FilesystemController::new(Arc::new(control_plane));
        assert_eq!(controller.answer_once(Duration::ZERO).await, Duration::ZERO);
    }

    #[tokio::test]
    async fn a_poll_that_was_held_open_has_already_spent_the_floor_it_would_have_waited() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_answer_one_query().returning(|| Ok(None));
        let controller = FilesystemController::new(Arc::new(control_plane));
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
            .expect_answer_one_query()
            .returning(|| Err(unreachable()));
        control_plane.expect_note().times(1).returning(|_| ());

        let controller = FilesystemController::new(Arc::new(control_plane));
        assert_eq!(
            controller.answer_once(Duration::ZERO).await,
            CONTROL_PLANE_BACKOFF
        );
    }
}
