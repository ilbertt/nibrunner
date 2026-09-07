//! What this host is running, as it last wrote it down.

use protocol::AppId;
use sqlx::SqliteConnection;

use crate::report::instance_record::InstanceRecord;
use crate::repositories::StoreError;

/// A record this daemon cannot read is left out rather than failing the load.
///
/// The same reasoning the JSON store had: a host whose notes were written by a different version
/// of this binary should come up serving what it can still understand, and re-derive the rest by
/// observing. What it must never do is refuse to start, because then nothing on it recovers.
pub async fn all(connection: &mut SqliteConnection) -> Result<Vec<InstanceRecord>, StoreError> {
    let rows = sqlx::query!("select record from instances order by app_id")
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::read)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| serde_json::from_str(&row.record).ok())
        .collect())
}

/// Replaced whole, because the caller holds every record and one it no longer has is an app it no
/// longer runs.
pub async fn replace_all(
    connection: &mut SqliteConnection,
    records: &[InstanceRecord],
) -> Result<(), StoreError> {
    sqlx::query!("delete from instances")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::write)?;
    for record in records {
        let app_id = record.app_id.as_str();
        let rendered =
            serde_json::to_string(record).map_err(|error| StoreError::Unwritable(error.to_string()))?;
        sqlx::query!(
            "insert into instances (app_id, record) values (?, ?)",
            app_id,
            rendered
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::write)?;
    }
    Ok(())
}

/// What an operator sees without a JSON tool, from the generated columns rather than a second copy.
pub async fn summary(connection: &mut SqliteConnection) -> Result<Vec<(AppId, String)>, StoreError> {
    let rows = sqlx::query!("select app_id, state from instances order by app_id")
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::read)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| Some((AppId::parse(row.app_id).ok()?, row.state?)))
        .collect())
}
