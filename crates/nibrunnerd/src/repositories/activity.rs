use std::collections::BTreeMap;

use protocol::AppId;
use sqlx::SqliteConnection;

use crate::repositories::StoreError;

pub async fn all(connection: &mut SqliteConnection) -> Result<BTreeMap<AppId, i64>, StoreError> {
    let rows = sqlx::query!("select app_id, last_active_at_ms from activity")
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::read)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| Some((AppId::parse(row.app_id).ok()?, row.last_active_at_ms)))
        .collect())
}

pub async fn replace_all(
    connection: &mut SqliteConnection,
    activity: &BTreeMap<AppId, i64>,
) -> Result<(), StoreError> {
    sqlx::query!("delete from activity")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::write)?;
    for (app_id, at_ms) in activity {
        let app_id = app_id.as_str();
        sqlx::query!(
            "insert into activity (app_id, last_active_at_ms) values (?, ?)",
            app_id,
            at_ms
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::write)?;
    }
    Ok(())
}
