use std::time::Duration;

use nibrunnerd::domain::filesystem::reader::guest_vsock_path;
use nibrunnerd::domain::sqlite::client::{GuestSqlite, GuestSqliteError};
use nibrunnerd::domain::sqlite::sessions::SqliteSessions;
use nibrunnerd::test_support::machine::{RunningHost, Tenant};
use protocol::{
    GuestPath, HranaPipelineRequest, HranaPipelineResponse, InstanceState, SqliteOperation, SqliteOutcome,
    SqliteQuery, SqliteSessionId,
};
use sqlx::{Connection, Executor};

async fn seeded(host: &RunningHost) -> Tenant {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let mut database = sqlx::SqliteConnection::connect_with(&options).await.unwrap();
    database
        .execute("CREATE TABLE kept (value INTEGER)")
        .await
        .unwrap();
    database.execute("PRAGMA wal_checkpoint(TRUNCATE)").await.unwrap();
    database
        .execute("INSERT INTO kept VALUES (9223372036854775807)")
        .await
        .unwrap();
    let mut archive = tar::Builder::new(Vec::new());
    for name in ["db.sqlite", "db.sqlite-wal", "db.sqlite-shm"] {
        let bytes = std::fs::read(directory.path().join(name)).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        archive.append_data(&mut header, name, bytes.as_slice()).unwrap();
    }
    let bytes = archive.into_inner().unwrap();
    let contents = nibrunnerd::test_support::initial_contents(&bytes, "/app/data");
    let store = std::path::Path::new(&host.host.config.artifact_store_url);
    let target = store.join(contents.object.object_key.as_str());
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, bytes).unwrap();
    let mut app = host.tenant(1);
    app.volume.initial_contents = Some(contents);
    database.close().await.unwrap();
    app
}

async fn connect(host: &RunningHost, app: &Tenant, path: &str) -> Result<GuestSqlite, GuestSqliteError> {
    GuestSqlite::connect(
        &guest_vsock_path(&host.host, &app.app_id),
        &GuestPath::parse("/app/data").unwrap(),
        &GuestPath::parse(path).unwrap(),
    )
    .await
}

async fn query(client: &mut GuestSqlite, sql: &str, baton: Option<String>) -> HranaPipelineResponse {
    let request: HranaPipelineRequest = serde_json::from_value(serde_json::json!({
        "baton": baton,
        "requests": [{"type": "execute", "stmt": {"sql": sql}}],
    }))
    .unwrap();
    client.pipeline(&request).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_reads_wal_data_and_refuses_writes_without_restarting_the_app() {
    let Some(host) = crate::host().await else { return };
    let app = seeded(&host).await;
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert_eq!(host.get(&app, "/remember").await.unwrap().body, "1");
    let mut client = connect(&host, &app, "/db.sqlite").await.unwrap();
    let response = query(&mut client, "SELECT value FROM kept", None).await;
    let encoded = serde_json::to_value(&response).unwrap();
    assert_eq!(
        encoded["results"][0]["response"]["result"]["rows"][0][0]["value"],
        "9223372036854775807"
    );
    let response = query(&mut client, "DELETE FROM kept", response.baton).await;
    assert_eq!(
        serde_json::to_value(&response).unwrap()["results"][0]["type"],
        "error"
    );
    let response = query(&mut client, "SELECT value FROM kept", response.baton).await;
    assert_eq!(
        serde_json::to_value(&response).unwrap()["results"][0]["response"]["result"]["rows"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(connect(&host, &app, "/etc/passwd").await.is_err());
    assert_eq!(host.get(&app, "/remember").await.unwrap().body, "2");
    drop(client);
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_worker_capacity_pressure_and_cancellation_leave_the_app_running() {
    let Some(host) = crate::host().await else { return };
    let app = seeded(&host).await;
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert_eq!(host.get(&app, "/remember").await.unwrap().body, "1");
    let mut clients = Vec::new();
    for _ in 0..guest_contract::sqlite::MAX_SESSIONS_PER_GUEST {
        clients.push(connect(&host, &app, "/db.sqlite").await.unwrap());
    }
    assert!(
        matches!(connect(&host, &app, "/db.sqlite").await, Err(GuestSqliteError::Refused { code, .. }) if code == "SQLITE_BUSY")
    );
    let mut pressure = clients.pop().unwrap();
    let request: HranaPipelineRequest = serde_json::from_value(serde_json::json!({
        "requests": [{"type":"execute","stmt":{"sql":"WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) SELECT hex(zeroblob(60000)) FROM n ORDER BY random()"}}],
    })).unwrap();
    match pressure.pipeline(&request).await {
        Ok(response) => assert_eq!(
            serde_json::to_value(response).unwrap()["results"][0]["type"],
            "error"
        ),
        Err(GuestSqliteError::Unavailable) => {}
        Err(error) => panic!("the bounded reader failed unexpectedly: {error}"),
    }
    drop(pressure);
    drop(clients);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut client = connect(&host, &app, "/db.sqlite").await.unwrap();
    let request: HranaPipelineRequest = serde_json::from_value(serde_json::json!({
        "requests": [{"type":"execute","stmt":{"sql":"WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n) SELECT sum(x) FROM n"}}],
    })).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client.pipeline(&request))
            .await
            .is_err()
    );
    drop(client);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut client = connect(&host, &app, "/db.sqlite").await.unwrap();
    let response = query(&mut client, "SELECT count(*) FROM kept", None).await;
    assert_eq!(
        serde_json::to_value(response).unwrap()["results"][0]["type"],
        "ok"
    );
    assert_eq!(host.get(&app, "/remember").await.unwrap().body, "2");
    drop(client);
    host.stop().await;
}

fn operation(app: &Tenant, operation: SqliteOperation) -> SqliteQuery {
    SqliteQuery {
        app_id: app.app_id.clone(),
        deployment_id: app.instance.deployment_id.clone(),
        session_id: SqliteSessionId::parse("database-session").unwrap(),
        operation,
    }
}

fn open() -> SqliteOperation {
    SqliteOperation::Open {
        root: GuestPath::parse("/app/data").unwrap(),
        path: GuestPath::parse("/db.sqlite").unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_database_session_wakes_and_pins_its_app_and_expires_when_the_deployment_changes() {
    let Some(host) = crate::host().await else { return };
    let app = seeded(&host).await.on_request(60_000);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    host.let_sleep(&app).await;
    let sessions = std::sync::Arc::new(SqliteSessions::default());
    assert!(matches!(
        sessions.query(&host.host, operation(&app, open())).await,
        SqliteOutcome::Opened
    ));
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert!(host.host.state.guest_busy(&app.app_id));
    nibrunnerd::test_support::measured_quiet_since(
        &host.host.state,
        &app.app_id,
        nibrunnerd::clock::now_ms() - 120_000,
    )
    .await;
    nibrunnerd::domain::reconcile::idle::apply_sleep(&host.host).await;
    assert_eq!(
        host.instance(&app.app_id).await.unwrap().state,
        InstanceState::Running
    );
    let replacement = app.clone().redeployed("sqlite-next");
    host.deploy(std::slice::from_ref(&replacement)).await;
    host.until("the new deployment to own the app", |report| {
        report.instances.iter().any(|instance| {
            instance.app_id == app.app_id
                && instance.deployment_id == replacement.instance.deployment_id
                && matches!(instance.state, InstanceState::Idle | InstanceState::Running)
        })
    })
    .await;
    assert!(!host.host.state.guest_busy(&app.app_id));
    let body: HranaPipelineRequest = serde_json::from_value(
        serde_json::json!({"requests":[{"type":"execute","stmt":{"sql":"SELECT value FROM kept"}}]}),
    )
    .unwrap();
    assert!(matches!(
        sessions
            .query(
                &host.host,
                operation(&app, SqliteOperation::Pipeline { body: body.clone() })
            )
            .await,
        SqliteOutcome::Failed { .. }
    ));
    assert!(matches!(
        sessions.query(&host.host, operation(&replacement, open())).await,
        SqliteOutcome::Opened
    ));
    let response = sessions
        .query(
            &host.host,
            operation(&replacement, SqliteOperation::Pipeline { body }),
        )
        .await;
    let SqliteOutcome::Responded { body } = response else {
        panic!("the replacement reads its persistent database")
    };
    assert_eq!(
        serde_json::to_value(body).unwrap()["results"][0]["response"]["result"]["rows"][0][0]["value"],
        "9223372036854775807"
    );
    assert!(matches!(
        sessions
            .query(&host.host, operation(&replacement, SqliteOperation::Close))
            .await,
        SqliteOutcome::Closed
    ));
    assert!(!host.host.state.guest_busy(&app.app_id));
    host.stop().await;
}
