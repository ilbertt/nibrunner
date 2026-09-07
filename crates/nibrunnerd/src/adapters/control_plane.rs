use std::time::Duration;

use protocol::{
    agent_routes, AgentSession, AgentSessionRequest, DesiredStateRequest, HostCapacity, HostDesiredState,
    HostReportedState, HostVersions, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER,
};

#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("{route} was not reached: {reason}")]
    Unreachable { route: String, reason: String },
    #[error("{route} answered {status}: {body}")]
    Refused {
        route: String,
        status: u16,
        body: String,
    },
    #[error("{route} answered with a message that does not match the protocol: {reason}")]
    Mismatch { route: String, reason: String },
}

impl ControlPlaneError {
    pub fn message(&self) -> String {
        self.to_string()
    }

    pub fn is_session_expired(&self) -> bool {
        matches!(self, ControlPlaneError::Refused { status: 401, .. })
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: usize = 256;

pub struct ControlPlaneClient {
    base_url: String,
    http: reqwest::Client,
}

impl ControlPlaneClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        crate::install_crypto_provider();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("a client with no TLS roots to load is always buildable"),
        }
    }

    fn url(&self, route: &str) -> String {
        format!("{}{}{}", self.base_url, protocol::AGENT_API_PREFIX, route)
    }

    async fn post<Body: serde::Serialize, Reply: serde::de::DeserializeOwned>(
        &self,
        route: &'static str,
        body: &Body,
        session_token: Option<&protocol::SecretString>,
    ) -> Result<Reply, ControlPlaneError> {
        let mut request = self
            .http
            .post(self.url(route))
            .header(PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION.to_string())
            .json(body);
        if let Some(token) = session_token {
            request = request.header("authorization", format!("Bearer {}", token.expose()));
        }
        let response = request
            .send()
            .await
            .map_err(|error| ControlPlaneError::Unreachable {
                route: route.to_string(),
                reason: error.to_string(),
            })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ControlPlaneError::Refused {
                route: route.to_string(),
                status: status.as_u16(),
                body: protocol::truncate_chars(text, MAX_BODY),
            });
        }
        serde_json::from_str(&text).map_err(|error| ControlPlaneError::Mismatch {
            route: route.to_string(),
            reason: error.to_string(),
        })
    }

    pub async fn open_session(
        &self,
        request: &AgentSessionRequest,
    ) -> Result<AgentSession, ControlPlaneError> {
        self.post(agent_routes::SESSION, request, None).await
    }

    pub async fn fetch_desired_state(
        &self,
        session_token: &protocol::SecretString,
    ) -> Result<HostDesiredState, ControlPlaneError> {
        self.post(
            agent_routes::DESIRED_STATE,
            &DesiredStateRequest::default(),
            Some(session_token),
        )
        .await
    }

    pub async fn fetch_filesystem_query(
        &self,
        session_token: &protocol::SecretString,
        served_app_ids: Vec<protocol::AppId>,
    ) -> Result<protocol::FilesystemQueryResponse, ControlPlaneError> {
        self.post(
            agent_routes::FILESYSTEM_QUERY,
            &protocol::FilesystemQueryRequest { served_app_ids },
            Some(session_token),
        )
        .await
    }

    pub async fn send_filesystem_query_result(
        &self,
        session_token: &protocol::SecretString,
        result: &protocol::FilesystemQueryResult,
    ) -> Result<(), ControlPlaneError> {
        let _: serde::de::IgnoredAny = self
            .post(agent_routes::FILESYSTEM_QUERY_RESULT, result, Some(session_token))
            .await
            .or_else(|error| match error {
                ControlPlaneError::Mismatch { .. } => Ok(serde::de::IgnoredAny),
                other => Err(other),
            })?;
        Ok(())
    }

    pub async fn send_reported_state(
        &self,
        session_token: &protocol::SecretString,
        report: &HostReportedState,
    ) -> Result<(), ControlPlaneError> {
        let _: serde::de::IgnoredAny = self
            .post(agent_routes::REPORTED_STATE, report, Some(session_token))
            .await
            .or_else(|error| match error {
                ControlPlaneError::Mismatch { .. } => Ok(serde::de::IgnoredAny),
                other => Err(other),
            })?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct HostIdentity {
    pub versions: HostVersions,
    pub capacity: HostCapacity,
}

pub async fn poll_once(
    client: &ControlPlaneClient,
    desired_state_file: &std::path::Path,
    session_token: &protocol::SecretString,
) -> Result<bool, ControlPlaneError> {
    let desired = client.fetch_desired_state(session_token).await?;
    let held = crate::desired::read_desired_state(desired_state_file)
        .ok()
        .flatten();
    if held.as_ref() == Some(&desired) {
        return Ok(false);
    }
    crate::desired::cache_desired_state(desired_state_file, &desired).map_err(|error| {
        ControlPlaneError::Unreachable {
            route: "the desired state file".into(),
            reason: error.message(),
        }
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{desired_instance, desired_state};

    #[test]
    fn every_route_is_built_from_the_protocol_prefix_rather_than_spelled_out() {
        let client = ControlPlaneClient::new("https://api.example.com/");
        assert_eq!(
            client.url(agent_routes::DESIRED_STATE),
            "https://api.example.com/internal/agent/desired-state"
        );
        assert_eq!(
            client.url(agent_routes::SESSION),
            "https://api.example.com/internal/agent/session"
        );
        assert_eq!(
            client.url(agent_routes::REPORTED_STATE),
            "https://api.example.com/internal/agent/reported-state"
        );
    }

    #[test]
    fn a_session_that_expired_is_a_round_trip_rather_than_a_fault() {
        let expired = ControlPlaneError::Refused {
            route: "/session".into(),
            status: 401,
            body: String::new(),
        };
        assert!(expired.is_session_expired());
        let refused = ControlPlaneError::Refused {
            route: "/session".into(),
            status: 500,
            body: String::new(),
        };
        assert!(!refused.is_session_expired());
    }

    #[tokio::test]
    async fn a_poll_that_changed_nothing_does_not_touch_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desired.json");
        let state = desired_state(|state| state.instances = vec![desired_instance(|_| {})]);
        crate::desired::cache_desired_state(&path, &state).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let held = crate::desired::read_desired_state(&path).unwrap();
        assert_eq!(held.as_ref(), Some(&state));
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
    }
}
