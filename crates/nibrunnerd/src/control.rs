//! The remote control plane, as an addon rather than a second input.
//!
//! It polls nibrun's own agent routes and writes what comes back into the file the daemon
//! watches. That is the whole of the integration: the reconciler has one source and cannot learn
//! which of them produced the document, a host that loses the control plane goes on converging on
//! the last one it was given, and this can be run as a separate process or not at all.
//!
//! v1 ships the client types, the shape of the loop, and the filesystem-query channel. What is
//! not here is a session that renews and a report that goes back — the seams are named so that
//! adding them is this file and nothing else.

use std::path::PathBuf;
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

    /// A 401 is the session having expired, which is a round trip rather than a fault.
    pub fn is_session_expired(&self) -> bool {
        matches!(self, ControlPlaneError::Refused { status: 401, .. })
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: usize = 256;

/// Every call is an outbound POST carrying one JSON document, including the two that read: a
/// request body keeps them to exactly one wire format and one validation path.
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
        // The two programs ship on different pipelines, so every reply is validated before it is
        // believed rather than trusted for having arrived.
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

    /// A read somebody is waiting for, or nothing.
    ///
    /// The control plane holds this request open until a read arrives, so a poll that came back
    /// with nothing has already spent longer than any pause this side would add. The apps offered
    /// are sent on every poll rather than registered once: one torn down between two polls stops
    /// being offered on the next, with nothing to invalidate.
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

    /// Sent whatever the answer was, because a failure is the answer as far as whoever asked is
    /// concerned: a host that stays quiet turns a refusal somebody could act on into a timeout.
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

    /// Nothing comes back. The desired-state poll is the only channel carrying state to a host, so
    /// a second copy on this reply would be a second thing to keep true.
    pub async fn send_reported_state(
        &self,
        session_token: &protocol::SecretString,
        report: &HostReportedState,
    ) -> Result<(), ControlPlaneError> {
        let _: serde::de::IgnoredAny = self
            .post(agent_routes::REPORTED_STATE, report, Some(session_token))
            .await
            .or_else(|error| match error {
                // A 204 carries no body, which is not a message that fails to match the protocol.
                ControlPlaneError::Mismatch { .. } => Ok(serde::de::IgnoredAny),
                other => Err(other),
            })?;
        Ok(())
    }
}

/// What the poller needs to describe this host when it opens a session.
#[derive(Debug, Clone)]
pub struct HostIdentity {
    pub versions: HostVersions,
    pub capacity: HostCapacity,
}

/// The addon: poll, and write what came back into the file the daemon watches. Deliberately not
/// wired into the daemon's own loop — a host runs this beside it or not at all.
pub struct DesiredStatePoller {
    pub client: ControlPlaneClient,
    pub desired_state_file: PathBuf,
}

impl DesiredStatePoller {
    /// One poll. The write is what the daemon reacts to, so a poll that changed nothing costs a
    /// comparison and no reconcile.
    pub async fn poll_once(&self, session_token: &protocol::SecretString) -> Result<bool, ControlPlaneError> {
        let desired = self.client.fetch_desired_state(session_token).await?;
        let held = crate::desired::read_desired_state(&self.desired_state_file)
            .ok()
            .flatten();
        if held.as_ref() == Some(&desired) {
            return Ok(false);
        }
        crate::desired::cache_desired_state(&self.desired_state_file, &desired).map_err(|error| {
            ControlPlaneError::Unreachable {
                route: "the desired state file".into(),
                reason: error.message(),
            }
        })?;
        Ok(true)
    }
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

    /// The poller's whole contract with the daemon: it writes the file, and a document that did
    /// not move is not a write.
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
