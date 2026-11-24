use crate::domain::store::StoreError;
use crate::repositories::deleted_volumes_repository::decode_stored_report;
use crate::repositories::Repositories;

pub async fn import_documents(
    repositories: &Repositories,
    config: &crate::config::HostConfig,
) -> Result<(), StoreError> {
    if !repositories.holds_nothing().await? {
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
