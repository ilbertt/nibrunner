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

    #[test]
    fn a_route_that_could_not_be_reached_names_the_route_and_the_reason() {
        let unreachable = ControlPlaneError::Unreachable {
            route: "/desired-state".into(),
            reason: "connection refused".into(),
        };
        assert_eq!(
            unreachable.message(),
            "/desired-state was not reached: connection refused"
        );
        assert!(!unreachable.is_session_expired());
        let mismatch = ControlPlaneError::Mismatch {
            route: "/session".into(),
            reason: "missing field `hostId`".into(),
        };
        assert!(mismatch.message().contains("does not match the protocol"));
        assert!(!mismatch.is_session_expired());
    }

    #[derive(Clone)]
    struct Answer {
        status: u16,
        body: String,
    }

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Option<String>, String)>>>;

    async fn control_plane(answer: impl Fn(&str) -> Answer + Send + Sync + 'static) -> (String, Seen) {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let answer = std::sync::Arc::new(answer);
        tokio::spawn({
            let seen = seen.clone();
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = seen.clone();
                    let answer = answer.clone();
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(
                            move |request: hyper::Request<hyper::body::Incoming>| {
                                let seen = seen.clone();
                                let answer = answer.clone();
                                async move {
                                    let path = request.uri().path().to_string();
                                    let version = request
                                        .headers()
                                        .get(PROTOCOL_VERSION_HEADER)
                                        .and_then(|value| value.to_str().ok())
                                        .unwrap_or_default()
                                        .to_string();
                                    let authorization = request
                                        .headers()
                                        .get("authorization")
                                        .and_then(|value| value.to_str().ok())
                                        .map(str::to_string);
                                    let payload = http_body_util::BodyExt::collect(request.into_body())
                                        .await
                                        .map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned())
                                        .unwrap_or_default();
                                    let given = answer(&path);
                                    seen.lock().expect("no panic holds this lock").push((
                                        path,
                                        version,
                                        authorization,
                                        payload,
                                    ));
                                    Ok::<_, std::convert::Infallible>(
                                        hyper::Response::builder()
                                            .status(given.status)
                                            .body(http_body_util::Full::new(bytes::Bytes::from(given.body)))
                                            .unwrap(),
                                    )
                                }
                            },
                        );
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        });
        (base, seen)
    }

    fn token() -> protocol::SecretString {
        protocol::SecretString::parse("a-session-token").unwrap()
    }

    fn a_session() -> AgentSession {
        AgentSession {
            host_id: crate::test_support::host_id(),
            session_token: token(),
            expires_at: crate::test_support::observed_at(),
            poll: protocol::DEFAULT_AGENT_POLL_SETTINGS,
        }
    }

    fn session_request() -> AgentSessionRequest {
        AgentSessionRequest {
            host_id: None,
            versions: HostVersions {
                agent: "0.1.0".into(),
                guest_image: "6.1.180".into(),
                zerofs: "0.1.0".into(),
                firecracker: "1.16.1".into(),
            },
            capacity: HostCapacity {
                vcpu_count: 4,
                memory_mib: 8192,
                cache_bytes: 0,
            },
        }
    }

    #[tokio::test]
    async fn a_session_is_opened_without_a_token_and_every_later_call_carries_the_one_it_gave() {
        let session = serde_json::to_string(&a_session()).unwrap();
        let state = serde_json::to_string(&desired_state(|_| {})).unwrap();
        let (base, seen) = control_plane(move |path| Answer {
            status: 200,
            body: if path.ends_with(agent_routes::SESSION) {
                session.clone()
            } else {
                state.clone()
            },
        })
        .await;
        let client = ControlPlaneClient::new(&base);

        assert_eq!(
            client.open_session(&session_request()).await.unwrap(),
            a_session()
        );
        client.fetch_desired_state(&token()).await.unwrap();

        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls[0].0, "/internal/agent/session");
        assert_eq!(calls[0].1, PROTOCOL_VERSION.to_string());
        assert_eq!(calls[0].2, None, "a session is asked for without one");
        assert!(calls[0].3.contains("firecracker"));
        assert_eq!(calls[1].0, "/internal/agent/desired-state");
        assert_eq!(calls[1].2.as_deref(), Some("Bearer a-session-token"));
    }

    #[tokio::test]
    async fn a_session_the_control_plane_no_longer_honours_is_told_apart_from_a_fault() {
        let (base, _) = control_plane(|_| Answer {
            status: 401,
            body: "the session has expired".into(),
        })
        .await;
        let error = ControlPlaneClient::new(&base)
            .fetch_desired_state(&token())
            .await
            .unwrap_err();
        assert!(error.is_session_expired(), "{error}");
        assert!(error.message().contains("the session has expired"), "{error}");

        let (base, _) = control_plane(|_| Answer {
            status: 503,
            body: "try later".into(),
        })
        .await;
        let error = ControlPlaneClient::new(&base)
            .send_reported_state(&token(), &report())
            .await
            .unwrap_err();
        assert!(!error.is_session_expired(), "{error}");
    }

    fn report() -> HostReportedState {
        HostReportedState {
            host_id: crate::test_support::host_id(),
            reported_at: crate::test_support::observed_at(),
            state: protocol::HostState::Ready,
            versions: session_request().versions,
            capacity: session_request().capacity,
            allocatable: session_request().capacity,
            instances: vec![],
            volumes: vec![],
            checkpoints: vec![],
            exports: vec![],
        }
    }

    #[tokio::test]
    async fn a_control_plane_that_answers_a_report_with_nothing_has_still_taken_it() {
        let (base, seen) = control_plane(|_| Answer {
            status: 204,
            body: String::new(),
        })
        .await;
        let client = ControlPlaneClient::new(&base);
        client.send_reported_state(&token(), &report()).await.unwrap();
        client
            .send_filesystem_query_result(
                &token(),
                &protocol::FilesystemQueryResult {
                    query_id: protocol::FilesystemQueryId::parse("fsq-1").unwrap(),
                    outcome: protocol::FilesystemQueryOutcome::Failed {
                        message: "the app is not running".into(),
                    },
                },
            )
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_document_that_is_not_the_shape_the_protocol_names_is_a_mismatch_not_a_guess() {
        let (base, _) = control_plane(|_| Answer {
            status: 200,
            body: r#"{"hostId":"host-1"}"#.into(),
        })
        .await;
        let error = ControlPlaneClient::new(&base)
            .fetch_desired_state(&token())
            .await
            .unwrap_err();
        assert!(matches!(error, ControlPlaneError::Mismatch { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_control_plane_that_says_a_great_deal_about_a_refusal_is_cut_down_before_it_is_logged() {
        let long = "the control plane explained itself at length. ".repeat(20);
        let (base, _) = control_plane(move |_| Answer {
            status: 500,
            body: long.clone(),
        })
        .await;
        let error = ControlPlaneClient::new(&base)
            .fetch_desired_state(&token())
            .await
            .unwrap_err();
        match error {
            ControlPlaneError::Refused { body, status, .. } => {
                assert_eq!(status, 500);
                assert_eq!(body.chars().count(), MAX_BODY);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nothing_this_host_can_reach_is_a_refusal_it_could_act_on() {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let error = ControlPlaneClient::new(&base)
            .fetch_desired_state(&token())
            .await
            .unwrap_err();
        assert!(matches!(error, ControlPlaneError::Unreachable { .. }), "{error}");
        assert!(!error.is_session_expired());
    }

    #[tokio::test]
    async fn a_poll_writes_the_document_the_first_time_and_says_nothing_moved_the_second() {
        let state = serde_json::to_string(&desired_state(|state| {
            state.instances = vec![desired_instance(|_| {})]
        }))
        .unwrap();
        let (base, _) = control_plane(move |_| Answer {
            status: 200,
            body: state.clone(),
        })
        .await;
        let client = ControlPlaneClient::new(&base);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desired.json");

        assert!(poll_once(&client, &path, &token()).await.unwrap());
        let written = crate::desired::read_desired_state(&path).unwrap().unwrap();
        assert_eq!(written.instances.len(), 1);

        assert!(
            !poll_once(&client, &path, &token()).await.unwrap(),
            "a document that did not move is not a change to converge on"
        );
    }

    #[tokio::test]
    async fn a_document_that_cannot_be_written_down_is_not_reported_as_taken_in() {
        let state = serde_json::to_string(&desired_state(|_| {})).unwrap();
        let (base, _) = control_plane(move |_| Answer {
            status: 200,
            body: state.clone(),
        })
        .await;
        let directory = tempfile::tempdir().unwrap();
        let occupied = directory.path().join("state");
        std::fs::write(&occupied, b"a file, not a directory").unwrap();
        let error = poll_once(
            &ControlPlaneClient::new(&base),
            &occupied.join("desired.json"),
            &token(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ControlPlaneError::Unreachable { .. }), "{error}");
        assert!(error.message().contains("desired state file"), "{error}");
    }

    #[tokio::test]
    async fn a_cached_document_this_host_cannot_read_is_taken_in_again_rather_than_trusted() {
        let state = serde_json::to_string(&desired_state(|_| {})).unwrap();
        let (base, _) = control_plane(move |_| Answer {
            status: 200,
            body: state.clone(),
        })
        .await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desired.json");
        std::fs::write(&path, "{ not a document").unwrap();
        assert!(poll_once(&ControlPlaneClient::new(&base), &path, &token())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn every_app_this_host_serves_is_named_when_it_asks_what_is_wanted_of_them() {
        let (base, seen) = control_plane(|_| Answer {
            status: 200,
            body: r#"{"result":"none"}"#.into(),
        })
        .await;
        assert_eq!(
            ControlPlaneClient::new(&base)
                .fetch_filesystem_query(&token(), vec![crate::test_support::app_id()])
                .await
                .unwrap(),
            protocol::FilesystemQueryResponse::None
        );
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls[0].0, "/internal/agent/filesystem-query");
        assert!(calls[0].3.contains("app-1"), "{}", calls[0].3);
    }
}
