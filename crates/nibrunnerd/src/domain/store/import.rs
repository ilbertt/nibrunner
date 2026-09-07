use crate::domain::store::StoreError;
use crate::repositories::Repositories;

pub async fn import_documents(
    repositories: &Repositories,
    config: &crate::config::HostConfig,
) -> Result<(), StoreError> {
    if !repositories.holds_nothing().await {
        return Ok(());
    }

    let records = crate::domain::report::instance_record::read_instance_records(
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
