use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::Controller;
use crate::services::control_plane_service::ControlPlaneService;

pub const IDLE_POLL_FLOOR: Duration = Duration::from_secs(5);

pub const CONTROL_PLANE_BACKOFF: Duration = Duration::from_secs(15);

pub struct ControlPlaneController {
    control_plane: Arc<dyn ControlPlaneService>,
}

impl ControlPlaneController {
    pub fn new(control_plane: Arc<dyn ControlPlaneService>) -> Arc<Self> {
        Arc::new(Self { control_plane })
    }

    pub async fn poll_once(&self) -> Duration {
        match self.control_plane.poll_desired_state().await {
            Ok(true) => {
                tracing::info!("the control plane gave this host a new document");
                IDLE_POLL_FLOOR
            }
            Ok(false) => IDLE_POLL_FLOOR,
            Err(error) => {
                self.control_plane.note(&error).await;
                tracing::warn!(error = %error.message(), "the control plane could not be polled");
                CONTROL_PLANE_BACKOFF + IDLE_POLL_FLOOR
            }
        }
    }
}

#[async_trait]
impl Controller for ControlPlaneController {
    fn name(&self) -> &'static str {
        "control-plane"
    }

    async fn run(&self) {
        loop {
            tokio::time::sleep(self.poll_once().await).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::control_plane::ControlPlaneError;
    use crate::services::control_plane_service::MockControlPlaneService;

    pub(super) fn unreachable() -> ControlPlaneError {
        ControlPlaneError::Unreachable {
            route: "/agent/desired-state".into(),
            reason: "connection refused".into(),
        }
    }

    #[tokio::test]
    async fn a_document_the_control_plane_handed_over_waits_out_the_floor_before_asking_again() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane.expect_poll_desired_state().returning(|| Ok(true));
        let controller = ControlPlaneController::new(Arc::new(control_plane));
        assert_eq!(controller.poll_once().await, IDLE_POLL_FLOOR);
    }

    #[tokio::test]
    async fn a_control_plane_that_would_not_answer_is_backed_off_from_and_the_session_told() {
        let mut control_plane = MockControlPlaneService::new();
        control_plane
            .expect_poll_desired_state()
            .returning(|| Err(unreachable()));
        control_plane.expect_note().times(1).returning(|_| ());

        let controller = ControlPlaneController::new(Arc::new(control_plane));
        assert_eq!(
            controller.poll_once().await,
            CONTROL_PLANE_BACKOFF + IDLE_POLL_FLOOR
        );
    }
}
