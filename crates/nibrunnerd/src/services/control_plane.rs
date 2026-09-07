use std::sync::Arc;

use protocol::{
    AgentSessionRequest, FilesystemQuery, FilesystemQueryResponse, FilesystemQueryResult, HostCapacity,
    SecretString,
};
use tokio::sync::Mutex;

use crate::adapters::control_plane::{ControlPlaneClient, ControlPlaneError};
use crate::host::Host;
use crate::services::report::capacity::{read_filesystem_space, read_vcpu_count};

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

pub struct SessionHolder {
    client: ControlPlaneClient,
    held: Mutex<Option<SecretString>>,
}

impl SessionHolder {
    pub fn new(client: ControlPlaneClient) -> Self {
        Self {
            client,
            held: Mutex::new(None),
        }
    }

    pub fn client(&self) -> &ControlPlaneClient {
        &self.client
    }

    pub async fn current(&self, host: &Host) -> Result<SecretString, ControlPlaneError> {
        let mut held = self.held.lock().await;
        if let Some(token) = held.as_ref() {
            return Ok(token.clone());
        }
        let space = read_filesystem_space(&host.config.state_dir).unwrap_or_default();
        let session = self
            .client
            .open_session(&AgentSessionRequest {
                host_id: host.known_host_id().await,
                versions: crate::run::host_versions(host),
                capacity: HostCapacity {
                    vcpu_count: read_vcpu_count(),
                    memory_mib: host.guest_memory_mib,
                    cache_bytes: space.total_bytes,
                },
            })
            .await?;
        host.remember_host_id(session.host_id.as_str()).await;
        tracing::info!(host_id = %session.host_id, "a session with the control plane is open");
        *held = Some(session.session_token.clone());
        Ok(session.session_token)
    }

    pub async fn expired(&self) {
        self.held.lock().await.take();
    }

    pub async fn note(&self, error: &ControlPlaneError) {
        if error.is_session_expired() {
            self.expired().await;
        }
    }
}

pub async fn served_app_ids(host: &Host) -> Vec<protocol::AppId> {
    crate::services::filesystem::reader::served_app_ids(host).await
}

pub async fn answer_one_query(
    host: &Host,
    sessions: &Arc<SessionHolder>,
) -> Result<Option<FilesystemQuery>, ControlPlaneError> {
    let token = sessions.current(host).await?;
    let response = sessions
        .client()
        .fetch_filesystem_query(&token, served_app_ids(host).await)
        .await?;
    let FilesystemQueryResponse::Query { query } = response else {
        return Ok(None);
    };
    let result: FilesystemQueryResult = crate::services::filesystem::reader::answer(host, &query).await;
    sessions
        .client()
        .send_filesystem_query_result(&token, &result)
        .await?;
    Ok(Some(query))
}

pub async fn poll_desired_state(
    host: &Host,
    sessions: &Arc<SessionHolder>,
) -> Result<bool, ControlPlaneError> {
    let token = sessions.current(host).await?;
    crate::adapters::control_plane::poll_once(sessions.client(), &host.config.desired_state_file, &token)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    const NOWHERE: &str = "http://127.0.0.1:1";

    fn token(value: &str) -> SecretString {
        SecretString::parse(value).unwrap()
    }

    fn unreachable_sessions() -> Arc<SessionHolder> {
        Arc::new(SessionHolder::new(ControlPlaneClient::new(NOWHERE)))
    }

    async fn holding(value: &str) -> Arc<SessionHolder> {
        let sessions = unreachable_sessions();
        *sessions.held.lock().await = Some(token(value));
        sessions
    }

    fn refused(status: u16) -> ControlPlaneError {
        ControlPlaneError::Refused {
            route: protocol::agent_routes::DESIRED_STATE.to_string(),
            status,
            body: String::new(),
        }
    }

    fn unreachable() -> ControlPlaneError {
        ControlPlaneError::Unreachable {
            route: protocol::agent_routes::DESIRED_STATE.to_string(),
            reason: "connection refused".to_string(),
        }
    }

    #[tokio::test]
    async fn a_control_plane_that_cannot_be_reached_is_an_error_and_not_a_panic() {
        let host = test_host().await;
        let sessions = Arc::new(SessionHolder::new(ControlPlaneClient::new("http://127.0.0.1:1")));
        assert!(answer_one_query(host.arc(), &sessions).await.is_err());
    }

    #[tokio::test]
    async fn only_the_apps_this_host_holds_a_slot_for_are_offered() {
        let host = test_host().await;
        assert!(served_app_ids(host.arc()).await.is_empty());
        host.slot_for(&app_id()).await.unwrap();
        assert_eq!(served_app_ids(host.arc()).await, vec![app_id()]);
    }

    #[tokio::test]
    async fn a_session_already_held_is_handed_back_rather_than_a_second_one_opened() {
        let host = test_host().await;
        let sessions = holding("the-one-already-open").await;

        for _ in 0..3 {
            let held = sessions
                .current(&host)
                .await
                .expect("a held session needs no round trip to a control plane that is not there");
            assert_eq!(held.expose(), "the-one-already-open");
        }
    }

    #[tokio::test]
    async fn a_host_with_no_session_yet_has_to_open_one_and_says_so_when_it_cannot() {
        let host = test_host().await;
        let sessions = unreachable_sessions();
        assert!(sessions.current(&host).await.is_err());
        assert!(sessions.held.lock().await.is_none());
    }

    #[tokio::test]
    async fn a_session_the_control_plane_no_longer_accepts_is_let_go_of() {
        let sessions = holding("stale").await;
        sessions.note(&refused(401)).await;
        assert!(sessions.held.lock().await.is_none());
    }

    #[tokio::test]
    async fn a_failure_that_is_not_the_session_lapsing_leaves_it_held() {
        for error in [refused(500), refused(429), refused(403), unreachable()] {
            let sessions = holding("still-good").await;
            sessions.note(&error).await;
            assert_eq!(
                sessions.held.lock().await.as_ref().map(SecretString::expose),
                Some("still-good"),
                "{error} threw away a session that had not lapsed"
            );
        }
    }

    #[tokio::test]
    async fn a_session_can_be_let_go_of_without_an_error_to_prompt_it() {
        let sessions = holding("stale").await;
        sessions.expired().await;
        assert!(sessions.held.lock().await.is_none());
        sessions.expired().await;
        assert!(sessions.held.lock().await.is_none());
    }

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

    #[tokio::test]
    async fn a_document_that_never_arrived_is_never_written_down() {
        let host = test_host().await;
        assert!(poll_desired_state(host.arc(), &holding("open").await)
            .await
            .is_err());
        assert!(!host.config.desired_state_file.exists());
    }
}
