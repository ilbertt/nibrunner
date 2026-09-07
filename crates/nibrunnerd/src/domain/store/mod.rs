pub mod import;

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
