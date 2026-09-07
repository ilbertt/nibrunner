use std::sync::Arc;

use protocol::FilesystemQuery;

use crate::adapters::control_plane::ControlPlaneError;
use crate::domain::control_plane::{answer_one_query, poll_desired_state, SessionHolder};
use crate::host::Host;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait::async_trait]
pub trait ControlPlaneService: Send + Sync {
    async fn poll_desired_state(&self) -> Result<bool, ControlPlaneError>;
    async fn answer_one_query(&self) -> Result<Option<FilesystemQuery>, ControlPlaneError>;
    async fn note(&self, error: &ControlPlaneError);
}

pub struct RemoteControlPlane {
    host: Arc<Host>,
    sessions: Arc<SessionHolder>,
}

impl RemoteControlPlane {
    pub fn new(host: Arc<Host>, sessions: Arc<SessionHolder>) -> Arc<Self> {
        Arc::new(Self { host, sessions })
    }
}

#[async_trait::async_trait]
impl ControlPlaneService for RemoteControlPlane {
    async fn poll_desired_state(&self) -> Result<bool, ControlPlaneError> {
        poll_desired_state(&self.host, &self.sessions).await
    }

    async fn answer_one_query(&self) -> Result<Option<FilesystemQuery>, ControlPlaneError> {
        answer_one_query(&self.host, &self.sessions).await
    }

    async fn note(&self, error: &ControlPlaneError) {
        self.sessions.note(error).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SecretString;
    const NOWHERE: &str = "http://127.0.0.1:1";
    fn token(value: &str) -> SecretString {
        SecretString::parse(value).unwrap()
    }
    use crate::adapters::control_plane::ControlPlaneClient;
    fn unreachable_sessions() -> Arc<SessionHolder> {
        Arc::new(SessionHolder::new(ControlPlaneClient::new(NOWHERE)))
    }
    async fn holding(value: &str) -> Arc<SessionHolder> {
        let sessions = unreachable_sessions();
        *sessions.held.lock().await = Some(token(value));
        sessions
    }
    fn unreachable() -> ControlPlaneError {
        ControlPlaneError::Unreachable {
            route: protocol::agent_routes::DESIRED_STATE.to_string(),
            reason: "connection refused".to_string(),
        }
    }
    fn refused(status: u16) -> ControlPlaneError {
        ControlPlaneError::Refused {
            route: protocol::agent_routes::DESIRED_STATE.to_string(),
            status,
            body: String::new(),
        }
    }
    use crate::test_support::*;
    #[tokio::test]
    async fn the_remote_control_plane_lets_go_of_the_session_the_holder_was_keeping() {
        let host = test_host().await;
        let sessions = holding("stale").await;
        let remote = RemoteControlPlane::new(host.arc().clone(), sessions.clone());

        remote.note(&unreachable()).await;
        assert!(sessions.held.lock().await.is_some());
        remote.note(&refused(401)).await;
        assert!(sessions.held.lock().await.is_none());
    }
    #[tokio::test]
    async fn a_remote_control_plane_that_cannot_be_reached_refuses_rather_than_reporting_progress() {
        let host = test_host().await;
        let remote: Arc<dyn ControlPlaneService> =
            RemoteControlPlane::new(host.arc().clone(), unreachable_sessions());
        assert!(remote.poll_desired_state().await.is_err());
        assert!(remote.answer_one_query().await.is_err());
    }
}
