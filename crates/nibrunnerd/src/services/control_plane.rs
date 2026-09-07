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
}
