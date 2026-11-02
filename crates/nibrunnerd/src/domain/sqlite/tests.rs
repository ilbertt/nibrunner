use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

use super::*;
use crate::test_support::*;

struct Guest {
    listener: tokio::task::JoinHandle<()>,
    called: Arc<Notify>,
    count: Arc<AtomicUsize>,
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

async fn host() -> TestHost {
    let host = test_host().await;
    host.cache.lock().await.accept(desired_state(|desired| {
        desired.instances = vec![desired_instance(|_| {})]
    }));
    host.state.modify(|snapshot| snapshot.isolated = true).await;
    host.state
        .put_record(instance_record(|record| {
            record.state = InstanceState::Running;
            record.started_at = Some(protocol::Timestamp::now());
        }))
        .await;
    host.deployment_tasks
        .synchronize(&[(app_id(), deployment_id())])
        .await;
    host
}

async fn guest(host: &Host, hold: Option<Method>) -> Guest {
    let path = guest_vsock_path(host, &app_id());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let called = Arc::new(Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn({
        let called = called.clone();
        let count = count.clone();
        async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let called = called.clone();
                let count = count.clone();
                let hold = hold.clone();
                connections.spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut handshake = String::new();
                    reader.read_line(&mut handshake).await.unwrap();
                    let port = guest_contract::vsock::GUEST_SQLITE_VSOCK_PORT;
                    assert_eq!(handshake, guest_contract::vsock::connect_request(port));
                    reader.get_mut().write_all(format!("OK {port}\n").as_bytes()).await.unwrap();
                    let service = hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        let called = called.clone();
                        let count = count.clone();
                        let hold = hold.clone();
                        async move {
                            let method = request.method().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            if method == Method::POST { count.fetch_add(1, Ordering::SeqCst); }
                            if hold.as_ref() == Some(&method) {
                                called.notify_one();
                                std::future::pending::<()>().await;
                            }
                            let body = if method == Method::GET {
                                serde_json::json!({})
                            } else {
                                let request: HranaPipelineRequest = serde_json::from_slice(&bytes).unwrap();
                                serde_json::json!({"baton":"next", "base_url":null, "results":request.requests.iter().map(|_| serde_json::json!({"type":"ok", "response":{"type":"sequence"}})).collect::<Vec<_>>()})
                            };
                            Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap()))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(reader.into_inner()), service).await;
                });
            }
        }
    });
    Guest {
        listener: handle,
        called,
        count,
    }
}

fn request(id: &str, operation: SqliteOperation) -> SqliteQuery {
    SqliteQuery {
        app_id: app_id(),
        deployment_id: deployment_id(),
        session_id: SqliteSessionId::parse(id).unwrap(),
        operation,
    }
}

fn opening(id: &str) -> SqliteQuery {
    request(
        id,
        SqliteOperation::Open {
            root: GuestPath::parse("/app/data").unwrap(),
            path: GuestPath::parse("/db.sqlite").unwrap(),
        },
    )
}

fn pipeline(id: &str) -> SqliteQuery {
    request(
        id,
        SqliteOperation::Pipeline {
            body: serde_json::from_value(
                serde_json::json!({"requests":[{"type":"sequence","sql":"SELECT 1"}]}),
            )
            .unwrap(),
        },
    )
}

#[tokio::test]
async fn database_sessions_are_cancelled_and_drained_before_their_deployment_stops() {
    let host = host().await;
    let guest = guest(host.arc(), None).await;
    let sessions = Arc::new(SqliteSessions::default());
    assert!(matches!(
        sessions.query(host.arc(), opening("one")).await,
        SqliteOutcome::Opened
    ));
    assert!(host.state.guest_busy(&app_id()));
    assert!(matches!(
        sessions.query(host.arc(), pipeline("one")).await,
        SqliteOutcome::Responded { .. }
    ));
    assert_eq!(guest.count.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(1), host.deployment_tasks.stop(&app_id()))
        .await
        .unwrap();
    assert!(!host.state.guest_busy(&app_id()));
    assert!(lock(&sessions.entries).is_empty());
    assert!(matches!(
        sessions.query(host.arc(), pipeline("one")).await,
        SqliteOutcome::Failed { .. }
    ));
    assert!(matches!(
        sessions
            .query(host.arc(), request("one", SqliteOperation::Close))
            .await,
        SqliteOutcome::Closed
    ));
}

#[tokio::test]
async fn closing_a_session_bypasses_and_cancels_its_busy_query_without_retrying_sql() {
    let host = host().await;
    let guest = guest(host.arc(), Some(Method::POST)).await;
    let sessions = Arc::new(SqliteSessions::default());
    assert!(matches!(
        sessions.query(host.arc(), opening("one")).await,
        SqliteOutcome::Opened
    ));
    let query = tokio::spawn({
        let sessions = sessions.clone();
        let host = host.arc().clone();
        async move { sessions.query(&host, pipeline("one")).await }
    });
    guest.called.notified().await;
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(1),
            sessions.query(host.arc(), request("one", SqliteOperation::Close))
        )
        .await
        .unwrap(),
        SqliteOutcome::Closed
    ));
    assert!(matches!(query.await.unwrap(), SqliteOutcome::Failed { .. }));
    assert_eq!(guest.count.load(Ordering::SeqCst), 1);
    assert!(!host.state.guest_busy(&app_id()));
    assert!(lock(&sessions.entries).is_empty());
}

#[tokio::test]
async fn a_cancelled_open_releases_its_registration_activity_and_deployment_lease() {
    let host = host().await;
    let guest = guest(host.arc(), Some(Method::GET)).await;
    let sessions = Arc::new(SqliteSessions::default());
    let open = tokio::spawn({
        let sessions = sessions.clone();
        let host = host.arc().clone();
        async move { sessions.query(&host, opening("one")).await }
    });
    guest.called.notified().await;
    open.abort();
    let _ = open.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !lock(&sessions.entries).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!host.state.guest_busy(&app_id()));
    tokio::time::timeout(Duration::from_secs(1), host.deployment_tasks.shutdown())
        .await
        .unwrap();
}

#[tokio::test]
async fn stale_or_stopped_deployments_cannot_open_a_database_or_pin_the_app() {
    let host = host().await;
    let sessions = Arc::new(SqliteSessions::default());
    let mut stale = opening("stale");
    stale.deployment_id = DeploymentId::parse("dep-stale").unwrap();
    assert!(matches!(
        sessions.query(host.arc(), stale).await,
        SqliteOutcome::Failed { .. }
    ));
    host.cache.lock().await.accept(desired_state(|desired| {
        desired.instances = vec![desired_instance(|instance| {
            instance.desired_state = DesiredInstanceState::Stopped
        })]
    }));
    assert!(matches!(
        sessions.query(host.arc(), opening("stopped")).await,
        SqliteOutcome::Failed { .. }
    ));
    assert!(!host.state.guest_busy(&app_id()));
    assert!(lock(&sessions.entries).is_empty());
}

#[tokio::test]
async fn session_limits_and_owner_checks_cannot_close_another_apps_reader() {
    let host = host().await;
    let _guest = guest(host.arc(), None).await;
    let sessions = Arc::new(SqliteSessions::default());
    for id in ["one", "two", "three", "four"] {
        assert!(matches!(
            sessions.query(host.arc(), opening(id)).await,
            SqliteOutcome::Opened
        ));
    }
    assert!(
        matches!(sessions.query(host.arc(), opening("five")).await, SqliteOutcome::Failed { code, .. } if code == "SQLITE_BUSY")
    );
    assert!(
        matches!(sessions.query(host.arc(), opening("one")).await, SqliteOutcome::Failed { code, .. } if code == "SQLITE_MISUSE")
    );
    let mut foreign = request("one", SqliteOperation::Close);
    foreign.app_id = AppId::parse("app-2").unwrap();
    assert!(matches!(
        sessions.query(host.arc(), foreign).await,
        SqliteOutcome::Failed { .. }
    ));
    assert_eq!(lock(&sessions.entries).len(), 4);
    for id in ["one", "two", "three", "four"] {
        assert!(matches!(
            sessions
                .query(host.arc(), request(id, SqliteOperation::Close))
                .await,
            SqliteOutcome::Closed
        ));
    }
    assert!(!host.state.guest_busy(&app_id()));
}

#[tokio::test]
async fn an_idle_database_session_expires_and_releases_the_apps_sleep_guard() {
    let host = host().await;
    let _guest = guest(host.arc(), None).await;
    let sessions = Arc::new(SqliteSessions::default());
    assert!(matches!(
        sessions.query(host.arc(), opening("one")).await,
        SqliteOutcome::Opened
    ));
    tokio::time::pause();
    tokio::time::advance(guest_contract::sqlite::SESSION_IDLE_TIMEOUT + Duration::from_secs(1)).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !lock(&sessions.entries).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!host.state.guest_busy(&app_id()));
}
