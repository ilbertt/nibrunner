use std::collections::BTreeMap;

use protocol::{ReportedVolume, VolumeId};
use sqlx::SqliteConnection;

use crate::repositories::StoreError;

pub async fn all(
    connection: &mut SqliteConnection,
) -> Result<BTreeMap<VolumeId, ReportedVolume>, StoreError> {
    let rows = sqlx::query!("select report from deleted_volumes order by volume_id")
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::read)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let report: ReportedVolume = serde_json::from_str(&row.report).ok()?;
            Some((report.volume_id.clone(), report))
        })
        .collect())
}

pub async fn replace_all(
    connection: &mut SqliteConnection,
    deleted: &BTreeMap<VolumeId, ReportedVolume>,
) -> Result<(), StoreError> {
    sqlx::query!("delete from deleted_volumes")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::write)?;
    for (volume_id, report) in deleted {
        let volume_id = volume_id.as_str();
        let rendered =
            serde_json::to_string(report).map_err(|error| StoreError::Unwritable(error.to_string()))?;
        sqlx::query!(
            "insert into deleted_volumes (volume_id, report) values (?, ?)",
            volume_id,
            rendered
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::write)?;
    }
    Ok(())
}
