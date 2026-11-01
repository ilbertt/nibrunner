use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use protocol::{HranaNamedArg, HranaSql, HranaStatement, HranaValue};
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::{Database, Session};

static ONE_DATABASE: Mutex<()> = Mutex::new(());

struct Fixture {
    database: Database,
    writer: Connection,
    path: PathBuf,
    _directory: tempfile::TempDir,
    _one: MutexGuard<'static, ()>,
}

fn fixture() -> Fixture {
    let one = ONE_DATABASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().canonicalize().unwrap().join("database.sqlite");
    let writer = Connection::open(&path).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer
        .execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY, text TEXT, data BLOB);\
      INSERT INTO items VALUES(9223372036854775807, 'hello', x'0001ff');",
        )
        .unwrap();
    let database = Database::open(&path, || false).unwrap();
    Fixture {
        database,
        writer,
        path,
        _directory: directory,
        _one: one,
    }
}

fn statement(sql: &str) -> HranaStatement {
    HranaStatement {
        source: HranaSql {
            sql: Some(sql.into()),
            sql_id: None,
        },
        args: vec![],
        named_args: vec![],
        want_rows: None,
    }
}

fn execute(database: &mut Database, sql: &str) -> Value {
    database.begin_request();
    serde_json::to_value(database.execute(sql, &statement(sql)).unwrap()).unwrap()
}

#[test]
fn integers_and_binary_values_reach_hrana_without_precision_loss() {
    let mut fixture = fixture();
    let result = execute(
        &mut fixture.database,
        "SELECT id, text, data, NULL, 1.25 FROM items",
    );
    assert_eq!(
        result["rows"][0],
        json!([
            {"type":"integer","value":"9223372036854775807"},
            {"type":"text","value":"hello"},
            {"type":"blob","base64":"AAH/"},
            {"type":"null"},
            {"type":"float","value":1.25}
        ])
    );
}

#[test]
fn named_and_positional_parameters_are_bound_without_interpolating_sql() {
    let mut fixture = fixture();
    let sql = "SELECT ?1, :text, @blob, $unbound";
    let mut request = statement(sql);
    request.args = vec![HranaValue::Integer {
        value: "-9223372036854775808".into(),
    }];
    request.named_args = vec![
        HranaNamedArg {
            name: "text".into(),
            value: HranaValue::Text {
                value: "'; DROP TABLE items;--".into(),
            },
        },
        HranaNamedArg {
            name: "blob".into(),
            value: HranaValue::Blob {
                base64: "AAH/".into(),
            },
        },
        HranaNamedArg {
            name: "unbound".into(),
            value: HranaValue::Null,
        },
    ];
    let result = serde_json::to_value(fixture.database.execute(sql, &request).unwrap()).unwrap();
    assert_eq!(result["rows"][0][0]["value"], "-9223372036854775808");
    assert_eq!(result["rows"][0][1]["value"], "'; DROP TABLE items;--");
    assert_eq!(result["rows"][0][2]["base64"], "AAH/");
    assert_eq!(result["rows"][0][3]["type"], "null");
    assert_eq!(
        execute(&mut fixture.database, "SELECT count(*) FROM items")["rows"][0][0]["value"],
        "1"
    );
}

#[test]
fn named_parameters_override_positional_values_and_missing_parameters_are_rejected() {
    let mut fixture = fixture();
    let sql = "SELECT :value";
    let mut request = statement(sql);
    assert!(fixture.database.execute(sql, &request).is_err());
    request.args = vec![HranaValue::Integer { value: "1".into() }];
    request.named_args = vec![HranaNamedArg {
        name: "value".into(),
        value: HranaValue::Integer { value: "2".into() },
    }];
    let result = serde_json::to_value(fixture.database.execute(sql, &request).unwrap()).unwrap();
    assert_eq!(result["rows"][0][0]["value"], "2");
}

#[test]
fn writes_attachments_unsafe_pragmas_and_extensions_are_refused() {
    let mut fixture = fixture();
    for sql in [
        "INSERT INTO items VALUES(1,'changed',NULL)",
        "DELETE FROM items",
        "CREATE TABLE new_table(id)",
        "PRAGMA query_only = OFF",
        "PRAGMA writable_schema = ON",
        "ATTACH DATABASE ':memory:' AS other",
        "SELECT load_extension('/tmp/extension')",
    ] {
        assert!(fixture.database.execute(sql, &statement(sql)).is_err(), "{sql}");
    }
    assert_eq!(
        fixture
            .writer
            .query_row("SELECT count(*) FROM items", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        execute(&mut fixture.database, "PRAGMA table_info(items)")["rows"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn a_read_only_connection_sees_new_wal_commits() {
    let mut fixture = fixture();
    assert_eq!(
        execute(&mut fixture.database, "SELECT count(*) FROM items")["rows"][0][0]["value"],
        "1"
    );
    fixture
        .writer
        .execute("INSERT INTO items VALUES(1,'new',NULL)", [])
        .unwrap();
    assert_eq!(
        execute(&mut fixture.database, "SELECT count(*) FROM items")["rows"][0][0]["value"],
        "2"
    );
}

#[test]
fn database_and_sidecar_symlinks_are_refused_before_sqlite_opens_them() {
    let fixture = fixture();
    let link = fixture.path.with_file_name("link.sqlite");
    std::os::unix::fs::symlink(&fixture.path, &link).unwrap();
    assert!(Database::open(&link, || false).is_err());
    let sidecar = PathBuf::from(format!("{}-journal", fixture.path.display()));
    std::os::unix::fs::symlink(&fixture.path, &sidecar).unwrap();
    assert!(Database::open(&fixture.path, || false).is_err());
}

#[test]
fn row_and_response_budgets_reject_large_results_without_truncating_them() {
    let mut fixture = fixture();
    for sql in [
        "WITH RECURSIVE t(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM t WHERE x<1001) SELECT x FROM t",
        "WITH RECURSIVE t(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM t WHERE x<1000) SELECT printf('%06000d', x) FROM t",
    ] {
        fixture.database.begin_request();
        let error = fixture.database.execute(sql, &statement(sql)).err().unwrap();
        assert_eq!(error.code, "SQLITE_TOOBIG");
    }
}

#[test]
fn a_single_execute_rejects_multiple_statements_and_keeps_semicolons_inside_literals() {
    let mut fixture = fixture();
    let sql = "SELECT 1; SELECT 2";
    assert!(fixture.database.execute(sql, &statement(sql)).is_err());
    assert_eq!(
        execute(&mut fixture.database, "SELECT ';'; -- comment")["rows"][0][0]["value"],
        ";"
    );
    let described = fixture.database.describe("SELECT :value AS result").unwrap();
    assert!(described.is_readonly);
    assert_eq!(described.params[0].name.as_deref(), Some(":value"));
}

#[test]
fn disconnecting_a_query_interrupts_sqlite_instead_of_replaying_it() {
    let fixture = fixture();
    let cancelled = Arc::new(AtomicBool::new(false));
    let observed = cancelled.clone();
    let mut database = Database::open(&fixture.path, move || observed.load(Ordering::Relaxed)).unwrap();
    cancelled.store(true, Ordering::Relaxed);
    let sql = "WITH RECURSIVE t(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM t) SELECT sum(x) FROM t";
    let error = database.execute(sql, &statement(sql)).err().unwrap();
    assert_eq!(error.code, "SQLITE_INTERRUPT");
}

#[test]
fn an_unbounded_query_hits_the_worker_deadline() {
    let mut fixture = fixture();
    fixture.database.begin_request();
    let sql = "WITH RECURSIVE t(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM t) SELECT sum(x) FROM t";
    let error = fixture.database.execute(sql, &statement(sql)).err().unwrap();
    assert_eq!(error.code, "SQLITE_INTERRUPT");
}

fn pipeline(session: &mut Session, body: Value) -> Value {
    serde_json::to_value(session.pipeline(serde_json::from_value(body).unwrap()).unwrap()).unwrap()
}

#[test]
fn hrana_streams_rotate_batons_and_reject_replays_before_dispatch() {
    let fixture = fixture();
    let mut session = Session::new(fixture.database);
    let first = pipeline(
        &mut session,
        json!({"baton":null,"requests":[
            {"type":"store_sql","sql_id":1,"sql":"SELECT 123"}
        ]}),
    );
    let second = pipeline(
        &mut session,
        json!({"baton":first["baton"],"requests":[
            {"type":"execute","stmt":{"sql_id":1}}, {"type":"close_sql","sql_id":1}
        ]}),
    );
    assert_eq!(
        second["results"][0]["response"]["result"]["rows"][0][0]["value"],
        "123"
    );
    assert_ne!(first["baton"], second["baton"]);
    let replay = json!({"baton":first["baton"],"requests":[
        {"type":"store_sql","sql_id":1,"sql":"SELECT 999"}
    ]});
    assert!(session.pipeline(serde_json::from_value(replay).unwrap()).is_err());
    let third = pipeline(
        &mut session,
        json!({"baton":second["baton"],"requests":[
            {"type":"execute","stmt":{"sql_id":1}}
        ]}),
    );
    assert_eq!(third["results"][0]["type"], "error");
}

#[test]
fn batch_conditions_distinguish_success_failure_and_skipped_steps() {
    let fixture = fixture();
    let mut session = Session::new(fixture.database);
    let result = pipeline(
        &mut session,
        json!({"requests":[{"type":"batch","batch":{"steps":[
            {"stmt":{"sql":"SELECT 1"}},
            {"condition":{"type":"ok","step":0},"stmt":{"sql":"SELECT 2"}},
            {"condition":{"type":"error","step":0},"stmt":{"sql":"SELECT 3"}},
            {"stmt":{"sql":"SELECT missing_column FROM items"}},
            {"condition":{"type":"and","conds":[{"type":"ok","step":1},{"type":"error","step":3}]},"stmt":{"sql":"SELECT 5"}}
        ]}}]}),
    );
    let batch = &result["results"][0]["response"]["result"];
    assert_eq!(batch["step_results"].as_array().unwrap().len(), 5);
    assert_eq!(batch["step_errors"].as_array().unwrap().len(), 5);
    assert_eq!(batch["step_results"][0]["rows"][0][0]["value"], "1");
    assert_eq!(batch["step_results"][1]["rows"][0][0]["value"], "2");
    assert!(batch["step_results"][2].is_null());
    assert!(batch["step_errors"][2].is_null());
    assert!(batch["step_errors"][3].is_object());
    assert_eq!(batch["step_results"][4]["rows"][0][0]["value"], "5");
}

#[test]
fn hrana_describe_sequence_and_close_share_one_read_only_connection() {
    let fixture = fixture();
    let mut session = Session::new(fixture.database);
    let result = pipeline(
        &mut session,
        json!({"requests":[
            {"type":"describe","sql":"SELECT :parameter AS result"},
            {"type":"sequence","sql":"BEGIN; SELECT count(*) FROM items; ROLLBACK"},
            {"type":"close"}
        ]}),
    );
    assert_eq!(
        result["results"][0]["response"]["result"]["params"][0]["name"],
        ":parameter"
    );
    assert_eq!(result["results"][1]["response"]["type"], "sequence");
    assert_eq!(result["results"][2]["response"]["type"], "close");
    assert_eq!(result["baton"], Value::Null);
    assert!(session.is_closed());
    assert!(session
        .pipeline(serde_json::from_value(json!({"requests":[]})).unwrap())
        .is_err());
}

#[test]
fn invalid_batch_references_are_rejected_before_any_request_runs() {
    let fixture = fixture();
    let mut session = Session::new(fixture.database);
    let bad = json!({"requests":[
        {"type":"store_sql","sql_id":1,"sql":"SELECT 1"},
        {"type":"batch","batch":{"steps":[{"condition":{"type":"ok","step":1},"stmt":{"sql":"SELECT 2"}}]}}
    ]});
    assert!(session.pipeline(serde_json::from_value(bad).unwrap()).is_err());
    let result = pipeline(
        &mut session,
        json!({"requests":[{"type":"execute","stmt":{"sql_id":1}}]}),
    );
    assert_eq!(result["results"][0]["type"], "error");
}
