use protocol::{AccessPolicy, HostDesiredState};
use serde_json::Value;
use sqlx::SqlitePool;

use super::StoreError;
use crate::domain::report::instance_record::InstanceRecord;

const ACCESS_POLICY_VERSION: i64 = 1;

pub(super) async fn migrate(pool: &SqlitePool) -> Result<(), StoreError> {
    let mut tx = pool
        .begin_with("begin immediate")
        .await
        .map_err(StoreError::write)?;
    let completed = sqlx::query!(
        "select version from state_data_migrations where version = ?",
        ACCESS_POLICY_VERSION
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::read)?;
    if completed.is_some() {
        return tx.commit().await.map_err(StoreError::write);
    }

    let records = sqlx::query!("select app_id, record from instances")
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::read)?;
    for row in records {
        let mut record: Value = serde_json::from_str(&row.record)
            .map_err(|error| StoreError::Unreadable(format!("instance {}: {error}", row.app_id)))?;
        let changed = add_public_access(&mut record)?;
        serde_json::from_value::<InstanceRecord>(record.clone())
            .map_err(|error| StoreError::Unreadable(format!("instance {}: {error}", row.app_id)))?;
        if changed {
            let rendered = record.to_string();
            sqlx::query!(
                "update instances set record = ? where app_id = ?",
                rendered,
                row.app_id
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        }
    }

    let accepted = sqlx::query!("select document, digest from accepted_document where only_row = 0")
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::read)?;
    if let Some(row) = accepted {
        let mut document: Value = serde_json::from_str(&row.document)
            .map_err(|error| StoreError::Unreadable(format!("accepted document: {error}")))?;
        let instances = document
            .get_mut("instances")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| StoreError::Unreadable("the accepted document has no instances array".into()))?;
        let mut changed = false;
        for instance in instances {
            changed |= add_public_access(instance)?;
        }
        serde_json::from_value::<HostDesiredState>(document.clone())
            .map_err(|error| StoreError::Unreadable(format!("accepted document: {error}")))?;
        if changed {
            let rendered = document.to_string();
            sqlx::query!(
                "update accepted_document set document = ? where only_row = 0",
                rendered
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        }
    }

    sqlx::query!(
        "insert into state_data_migrations (version) values (?)",
        ACCESS_POLICY_VERSION
    )
    .execute(&mut *tx)
    .await
    .map_err(StoreError::write)?;
    tx.commit().await.map_err(StoreError::write)
}

fn add_public_access(instance: &mut Value) -> Result<bool, StoreError> {
    let fields = instance
        .as_object_mut()
        .ok_or_else(|| StoreError::Unreadable("a stored instance is not an object".into()))?;
    if fields.contains_key("access") {
        return Ok(false);
    }
    // Before access policies existed, every stored instance was public.
    fields.insert("access".into(), serde_json::json!(AccessPolicy::Public));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::*;
    use crate::domain::store::open;
    use crate::repositories::Repositories;

    async fn connect(path: &Path) -> SqlitePool {
        SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await
            .unwrap()
    }

    async fn pre_access_database(path: &Path) -> SqlitePool {
        let migrations = tempfile::tempdir().unwrap();
        for (name, sql) in [
            (
                "0001_host_state.sql",
                include_str!("../../../migrations/0001_host_state.sql"),
            ),
            (
                "0002_cron_registrations.sql",
                include_str!("../../../migrations/0002_cron_registrations.sql"),
            ),
        ] {
            std::fs::write(migrations.path().join(name), sql).unwrap();
        }
        let pool = connect(path).await;
        sqlx::migrate::Migrator::new(migrations.path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("fixtures/pre_access_state.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    async fn records(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query_as("select app_id, record from instances order by app_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn accepted(pool: &SqlitePool) -> (String, String) {
        sqlx::query_as("select document, digest from accepted_document where only_row = 0")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn other_state(pool: &SqlitePool) -> Vec<Vec<String>> {
        let mut state = Vec::new();
        for query in [
            "select json_array(only_row, host_id) from host_identity",
            "select json_array(app_id, slot) from slots order by app_id",
            "select json_array(only_row, cursor) from slot_cursor",
            "select json_array(app_id, last_active_at_ms) from activity order by app_id",
            "select json_array(app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds) from meters order by app_id",
            "select json_array(volume_id, report) from deleted_volumes order by volume_id",
            "select json_array(app_id, deployment_id, crontab) from cron_tables order by app_id",
            "select json_array(app_id, job_index, schedule, command, has_environment) from cron_jobs order by app_id, job_index",
            "select json_array(app_id, job_index, name, value) from cron_job_environment order by app_id, job_index, name",
            "select json_array(version, description, installed_on, success, hex(checksum), execution_time) from _sqlx_migrations where version <= 2 order by version",
        ] {
            state.push(sqlx::query_scalar(query).fetch_all(pool).await.unwrap());
        }
        state
    }

    fn with_public_access(record: &str) -> Value {
        let mut record: Value = serde_json::from_str(record).unwrap();
        assert!(record
            .as_object_mut()
            .unwrap()
            .insert("access".into(), serde_json::json!({"kind": "public"}))
            .is_none());
        record
    }

    #[tokio::test]
    async fn upgrading_a_pre_access_database_preserves_every_record_and_all_other_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let pool = pre_access_database(&path).await;
        let before_records = records(&pool).await;
        let (before_document, digest) = accepted(&pool).await;
        let before_other = other_state(&pool).await;
        let expected_records: Vec<_> = before_records
            .iter()
            .map(|(_, record)| with_public_access(record))
            .collect();
        let mut expected_document: Value = serde_json::from_str(&before_document).unwrap();
        for instance in expected_document["instances"].as_array_mut().unwrap() {
            assert!(instance.get("access").is_none());
            instance["access"] = serde_json::json!({"kind": "public"});
        }
        pool.close().await;

        for _ in 0..2 {
            let pool = open(&path).await.unwrap();
            let after_records: Vec<Value> = records(&pool)
                .await
                .into_iter()
                .map(|(_, record)| serde_json::from_str(&record).unwrap())
                .collect();
            assert_eq!(after_records, expected_records);
            let (after_document, after_digest) = accepted(&pool).await;
            assert_eq!(
                serde_json::from_str::<Value>(&after_document).unwrap(),
                expected_document
            );
            assert_eq!(after_digest, digest);
            assert_eq!(other_state(&pool).await, before_other);
            let repositories = Repositories::sqlite(pool.clone());
            let loaded = repositories.instances.all().await.unwrap();
            assert_eq!(loaded.len(), 2);
            assert_eq!(loaded[0].state, protocol::InstanceState::Running);
            assert_eq!(loaded[1].state, protocol::InstanceState::Idle);
            assert!(loaded.iter().all(|record| record.access == AccessPolicy::Public));
            let loaded_document = repositories.accepted_document.read().await.unwrap().unwrap();
            assert_eq!(
                loaded_document.desired,
                serde_json::from_value::<HostDesiredState>(expected_document.clone()).unwrap()
            );
            assert_eq!(loaded_document.digest.as_str(), digest);
            assert_eq!(repositories.deleted_volumes.all().await.unwrap().len(), 1);
            assert_eq!(repositories.cron.all().await.unwrap().len(), 1);
            let completed: Vec<i64> = sqlx::query_scalar("select version from state_data_migrations")
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(completed, [ACCESS_POLICY_VERSION]);
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn an_invalid_explicit_policy_rolls_back_records_and_completion_so_the_upgrade_can_retry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let pool = pre_access_database(&path).await;
        let before_records = records(&pool).await;
        let (document, digest) = accepted(&pool).await;
        let mut invalid: Value = serde_json::from_str(&document).unwrap();
        invalid["instances"][1]["access"] = Value::Null;
        let invalid = invalid.to_string();
        sqlx::query("update accepted_document set document = ?")
            .bind(&invalid)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        assert!(open(&path).await.is_err());
        let pool = connect(&path).await;
        assert_eq!(records(&pool).await, before_records);
        assert_eq!(accepted(&pool).await, (invalid, digest));
        let completed: i64 = sqlx::query_scalar("select count(*) from state_data_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(completed, 0);
        sqlx::query("update accepted_document set document = ?")
            .bind(document)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let pool = open(&path).await.unwrap();
        assert_eq!(
            Repositories::sqlite(pool.clone())
                .instances
                .all()
                .await
                .unwrap()
                .len(),
            2
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn an_existing_jwt_policy_is_preserved_during_the_one_time_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let pool = pre_access_database(&path).await;
        let jwt = AccessPolicy::Jwt(crate::test_support::jwt::policy(protocol::JwtAlgorithm::EdDSA));
        let mut record: Value = serde_json::from_str(&records(&pool).await[1].1).unwrap();
        record["access"] = serde_json::to_value(&jwt).unwrap();
        let record = record.to_string();
        sqlx::query("update instances set record = ? where app_id = 'app-2'")
            .bind(&record)
            .execute(&pool)
            .await
            .unwrap();
        let (document, _) = accepted(&pool).await;
        let mut document: Value = serde_json::from_str(&document).unwrap();
        document["instances"][1]["access"] = serde_json::to_value(&jwt).unwrap();
        sqlx::query("update accepted_document set document = ?")
            .bind(document.to_string())
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let pool = open(&path).await.unwrap();
        assert_eq!(records(&pool).await[1].1, record);
        let repositories = Repositories::sqlite(pool.clone());
        assert_eq!(repositories.instances.all().await.unwrap()[1].access, jwt);
        assert_eq!(
            repositories
                .accepted_document
                .read()
                .await
                .unwrap()
                .unwrap()
                .desired
                .instances[1]
                .access,
            jwt
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn a_completed_upgrade_never_fills_missing_access_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let pool = pre_access_database(&path).await;
        let before_records = records(&pool).await;
        let (document, _) = accepted(&pool).await;
        pool.close().await;
        let pool = open(&path).await.unwrap();
        for (app_id, record) in &before_records {
            sqlx::query("update instances set record = ? where app_id = ?")
                .bind(record)
                .bind(app_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("update accepted_document set document = ?")
            .bind(&document)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let pool = open(&path).await.unwrap();
        assert_eq!(records(&pool).await, before_records);
        assert_eq!(accepted(&pool).await.0, document);
        for (_, record) in before_records {
            assert!(serde_json::from_str::<InstanceRecord>(&record).is_err());
        }
        assert!(Repositories::sqlite(pool.clone())
            .accepted_document
            .read()
            .await
            .is_err());
        pool.close().await;
    }
}
