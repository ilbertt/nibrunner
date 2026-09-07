pub mod activity;
pub mod deleted_volumes;
pub mod host_identity;
pub mod instances;
pub mod slots;

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

const MAX_CONNECTIONS: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path} could not be opened: {reason}")]
    Unopenable { path: String, reason: String },
    #[error("the host's own notes could not be read: {0}")]
    Unreadable(String),
    #[error("the host's own notes could not be written: {0}")]
    Unwritable(String),
}

impl StoreError {
    pub fn message(&self) -> String {
        self.to_string()
    }

    pub fn read(error: sqlx::Error) -> Self {
        Self::Unreadable(error.to_string())
    }

    pub fn write(error: sqlx::Error) -> Self {
        Self::Unwritable(error.to_string())
    }
}

pub async fn open(path: &Path) -> Result<SqlitePool, StoreError> {
    if let Some(parent) = path.parent() {
        crate::json_store::make_directory(parent, 0o700).map_err(|error| StoreError::Unopenable {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    }
    let unopenable = |error: sqlx::Error| StoreError::Unopenable {
        path: path.display().to_string(),
        reason: error.to_string(),
    };
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(options)
        .await
        .map_err(unopenable)?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|error| StoreError::Unopenable {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    Ok(pool)
}

pub async fn in_memory() -> SqlitePool {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!("nibrunner-test-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let options = SqliteConnectOptions::from_str(&format!("file:{name}?mode=memory&cache=shared"))
        .expect("a shared in-memory url")
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(MAX_CONNECTIONS)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await
        .expect("an in-memory database opens");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("the schema applies");
    pool
}

pub async fn import_documents(
    pool: &SqlitePool,
    config: &crate::config::HostConfig,
) -> Result<(), StoreError> {
    let mut connection = pool.acquire().await.map_err(StoreError::read)?;
    let held: i64 = sqlx::query_scalar!(
        "select (select count(*) from slots) + (select count(*) from instances) as \"held!: i64\""
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::read)?;
    if held > 0 {
        return Ok(());
    }

    let records = crate::services::report::instance_record::read_instance_records(
        crate::json_store::read_json(&config.instances_file())
            .ok()
            .flatten(),
    );
    let assignments = crate::adapters::net::allocator::assignments_from(
        crate::json_store::read_json(&config.slots_file())
            .ok()
            .flatten()
            .unwrap_or_default(),
    );
    let cursor = crate::adapters::net::allocator::read_slot_cursor(
        crate::json_store::read_json(&config.slot_cursor_file())
            .ok()
            .flatten(),
    );
    let deleted: Vec<protocol::ReportedVolume> = crate::json_store::read_json(&config.deleted_volumes_file())
        .ok()
        .flatten()
        .unwrap_or_default();
    let activity_entries: Vec<serde_json::Value> = crate::json_store::read_json(&config.activity_file())
        .ok()
        .flatten()
        .unwrap_or_default();
    if records.is_empty() && assignments.is_empty() && deleted.is_empty() {
        return Ok(());
    }
    let last_active = activity_entries
        .iter()
        .filter_map(|entry| {
            let app_id = protocol::AppId::parse(entry.get("appId")?.as_str()?).ok()?;
            Some((app_id, entry.get("atMs")?.as_i64()?))
        })
        .collect();
    let deleted = deleted
        .into_iter()
        .map(|report| (report.volume_id.clone(), report))
        .collect();

    let mut tx = pool.begin().await.map_err(StoreError::write)?;
    instances::replace_all(&mut tx, &records).await?;
    slots::replace_all(&mut tx, &assignments).await?;
    slots::set_cursor(&mut tx, cursor).await?;
    activity::replace_all(&mut tx, &last_active).await?;
    deleted_volumes::replace_all(&mut tx, &deleted).await?;
    tx.commit().await.map_err(StoreError::write)?;
    tracing::info!(
        instances = records.len(),
        slots = assignments.len(),
        "what an earlier daemon wrote in documents was carried into the database"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_host_with_no_database_gets_one_with_the_schema_already_in_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let pool = open(&path).await.unwrap();
        assert!(path.exists());

        let tables: Vec<String> =
            sqlx::query_scalar("select name from sqlite_master where type = 'table' order by name")
                .fetch_all(&pool)
                .await
                .unwrap();
        for expected in [
            "activity",
            "deleted_volumes",
            "host_identity",
            "instances",
            "slot_cursor",
            "slots",
        ] {
            assert!(
                tables.contains(&expected.to_string()),
                "{expected} is missing: {tables:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_pass_that_did_not_finish_leaves_nothing_behind() {
        let pool = in_memory().await;
        let app_id = protocol::AppId::parse("app-1").unwrap();

        let mut tx = pool.begin().await.unwrap();
        instances::replace_all(&mut tx, &[crate::test_support::instance_record(|_| {})])
            .await
            .unwrap();
        slots::replace_all(&mut tx, &std::collections::BTreeMap::from([(app_id, 0)]))
            .await
            .unwrap();
        drop(tx);

        let mut connection = pool.acquire().await.unwrap();
        assert!(instances::all(&mut connection).await.unwrap().is_empty());
        assert!(slots::all(&mut connection).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn opening_a_database_that_already_exists_is_a_restart_and_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let first = open(&path).await.unwrap();
        sqlx::query("insert into slots (app_id, slot) values ('app-1', 0)")
            .execute(&first)
            .await
            .unwrap();
        first.close().await;

        let second = open(&path).await.unwrap();
        let held: i64 = sqlx::query_scalar("select count(*) from slots")
            .fetch_one(&second)
            .await
            .unwrap();
        assert_eq!(held, 1, "what the first daemon wrote is what the second finds");
    }
}
