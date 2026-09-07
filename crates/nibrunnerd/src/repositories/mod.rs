pub mod activity;
pub mod deleted_volumes;
pub mod host_identity;
pub mod instances;
pub mod slots;

use std::path::Path;
use std::sync::Arc;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

use crate::repositories::activity::{ActivityRepository, SqliteActivity};
use crate::repositories::deleted_volumes::{DeletedVolumeRepository, SqliteDeletedVolumes};
use crate::repositories::host_identity::{HostIdentityRepository, SqliteHostIdentity};
use crate::repositories::instances::{InstanceRepository, SqliteInstances};
use crate::repositories::slots::{SlotRepository, SqliteSlots};

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

pub struct Repositories {
    pub instances: Arc<dyn InstanceRepository>,
    pub slots: Arc<dyn SlotRepository>,
    pub activity: Arc<dyn ActivityRepository>,
    pub deleted_volumes: Arc<dyn DeletedVolumeRepository>,
    pub identity: Arc<dyn HostIdentityRepository>,
}

impl Repositories {
    pub fn sqlite(pool: SqlitePool) -> Self {
        Self {
            instances: Arc::new(SqliteInstances::new(pool.clone())),
            slots: Arc::new(SqliteSlots::new(pool.clone())),
            activity: Arc::new(SqliteActivity::new(pool.clone())),
            deleted_volumes: Arc::new(SqliteDeletedVolumes::new(pool.clone())),
            identity: Arc::new(SqliteHostIdentity::new(pool)),
        }
    }

    pub async fn holds_nothing(&self) -> bool {
        let slots = self.slots.all().await.map(|held| held.len()).unwrap_or_default();
        let instances = self
            .instances
            .all()
            .await
            .map(|held| held.len())
            .unwrap_or_default();
        slots + instances == 0
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
    repositories: &Repositories,
    config: &crate::config::HostConfig,
) -> Result<(), StoreError> {
    if !repositories.holds_nothing().await {
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

    repositories.slots.replace_all(&assignments, cursor).await?;
    repositories.instances.replace_all(&records).await?;
    repositories.activity.replace_all(&last_active).await?;
    repositories.deleted_volumes.replace_all(&deleted).await?;
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
    async fn opening_a_database_that_already_exists_is_a_restart_and_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let first = open(&path).await.unwrap();
        sqlx::query("insert into slots (app_id, slot) values ('app-1', 0)")
            .execute(&first)
            .await
            .unwrap();
        first.close().await;

        let second = Repositories::sqlite(open(&path).await.unwrap());
        assert_eq!(second.slots.all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_host_that_has_written_nothing_holds_nothing() {
        let repositories = Repositories::sqlite(in_memory().await);
        assert!(repositories.holds_nothing().await);
        repositories
            .instances
            .replace_all(&[crate::test_support::instance_record(|_| {})])
            .await
            .unwrap();
        assert!(!repositories.holds_nothing().await);
    }

    #[tokio::test]
    async fn a_slot_an_older_daemon_allocated_is_carried_over_rather_than_reissued() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::HostConfig::under(directory.path());
        crate::json_store::write_json(&config.slots_file(), &serde_json::json!({ "app-1": 3 })).unwrap();
        crate::json_store::write_json(&config.slot_cursor_file(), &serde_json::json!(4)).unwrap();

        let repositories = Repositories::sqlite(in_memory().await);
        import_documents(&repositories, &config).await.unwrap();
        assert_eq!(
            repositories
                .slots
                .all()
                .await
                .unwrap()
                .get(&crate::test_support::app_id()),
            Some(&3)
        );
        assert_eq!(repositories.slots.cursor().await.unwrap(), 4);
    }

    #[tokio::test]
    async fn a_database_that_already_holds_something_is_never_written_over_by_an_import() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::HostConfig::under(directory.path());
        crate::json_store::write_json(&config.slots_file(), &serde_json::json!({ "app-1": 3 })).unwrap();

        let repositories = Repositories::sqlite(in_memory().await);
        repositories
            .slots
            .replace_all(
                &std::collections::BTreeMap::from([(crate::test_support::app_id(), 7)]),
                8,
            )
            .await
            .unwrap();
        import_documents(&repositories, &config).await.unwrap();
        assert_eq!(
            repositories
                .slots
                .all()
                .await
                .unwrap()
                .get(&crate::test_support::app_id()),
            Some(&7)
        );
    }

    #[tokio::test]
    async fn a_host_with_no_documents_to_carry_over_imports_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::HostConfig::under(directory.path());
        let repositories = Repositories::sqlite(in_memory().await);
        import_documents(&repositories, &config).await.unwrap();
        assert!(repositories.holds_nothing().await);
    }
}
