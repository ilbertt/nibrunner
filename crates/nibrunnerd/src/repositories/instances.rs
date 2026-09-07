use protocol::AppId;
use sqlx::SqliteConnection;

use crate::repositories::StoreError;
use crate::services::report::instance_record::InstanceRecord;

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
