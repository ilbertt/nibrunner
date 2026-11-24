use crate::domain::store::StoreError;
use crate::repositories::deleted_volumes_repository::decode_stored_report;
use sqlx::SqlitePool;

pub async fn import_documents(
    pool: &SqlitePool,
    config: &crate::config::HostConfig,
) -> Result<(), StoreError> {
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(StoreError::write)?;
    let occupied = sqlx::query_scalar!(
        r#"select exists(select 1 from slots) or exists(select 1 from instances) as "held!: bool""#
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::read)?;
    if occupied {
        return Ok(());
    }

    let records =
        crate::json_store::read_json::<Vec<crate::domain::report::InstanceRecord>>(&config.instances_file())
            .map_err(|_| StoreError::Unreadable("could not read the legacy instance records".into()))?
            .unwrap_or_default();
    let raw = crate::json_store::read_json::<std::collections::BTreeMap<String, serde_json::Value>>(
        &config.slots_file(),
    )
    .map_err(|_| StoreError::Unreadable("could not read the legacy slot assignments".into()))?
    .unwrap_or_default();
    let count = raw.len();
    let assignments = crate::adapters::net::allocator::assignments_from(raw, config.max_apps);
    if assignments.len() != count {
        return Err(StoreError::Unreadable(
            "invalid or conflicting legacy slot assignments".into(),
        ));
    }
    let cursor = crate::json_store::read_json::<i64>(&config.slot_cursor_file())
        .map_err(|_| StoreError::Unreadable("could not read the legacy slot cursor".into()))?
        .unwrap_or_default();
    let deleted: Vec<_> =
        crate::json_store::read_json::<Vec<serde_json::Value>>(&config.deleted_volumes_file())
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .filter_map(decode_stored_report)
            .collect();
    let activity_entries: Vec<serde_json::Value> = crate::json_store::read_json(&config.activity_file())
        .ok()
        .flatten()
        .unwrap_or_default();
    if records.is_empty() && assignments.is_empty() && deleted.is_empty() {
        return Ok(());
    }
    for (app, slot) in &assignments {
        let id = app.as_str();
        let number = i64::from(*slot);
        sqlx::query!("insert into slots (app_id, slot) values (?, ?)", id, number)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
    }
    sqlx::query!(
        "insert or replace into slot_cursor (only_row, cursor) values (0, ?)",
        cursor
    )
    .execute(&mut *tx)
    .await
    .map_err(StoreError::write)?;
    for record in &records {
        let id = record.app_id.as_str();
        let json =
            serde_json::to_string(record).map_err(|error| StoreError::Unwritable(error.to_string()))?;
        sqlx::query!("insert into instances (app_id, record) values (?, ?)", id, json)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
    }
    for entry in activity_entries {
        let (Some(app_id), Some(at)) = (
            entry.get("appId").and_then(|v| v.as_str()),
            entry.get("atMs").and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        if protocol::AppId::parse(app_id).is_ok() {
            sqlx::query!(
                "insert or ignore into activity (app_id, last_active_at_ms) values (?, ?)",
                app_id,
                at
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        }
    }
    for report in deleted {
        let id = report.volume_id.as_str();
        let json =
            serde_json::to_string(&report).map_err(|error| StoreError::Unwritable(error.to_string()))?;
        sqlx::query!(
            "insert or ignore into deleted_volumes (volume_id, report) values (?, ?)",
            id,
            json
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::write)?;
    }
    tx.commit().await.map_err(StoreError::write)?;
    tracing::info!(
        instances = records.len(),
        slots = assignments.len(),
        "what an earlier daemon wrote in documents was carried into the database"
    );
    Ok(())
}
