use sqlx::{Sqlite, Transaction};

use crate::domain::store::StoreError;

pub(super) async fn ensure_app(tx: &mut Transaction<'_, Sqlite>, app_id: &str) -> Result<(), StoreError> {
    sqlx::query!(
        "insert into apps (app_id) values (?) on conflict (app_id) do nothing",
        app_id
    )
    .execute(&mut **tx)
    .await
    .map_err(StoreError::write)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;

    use crate::domain::store::{in_memory, open};
    use crate::repositories::Repositories;
    use crate::test_support::{app_id, instance_record, reported_volume, volume_id};

    async fn persisted_rows(pool: &SqlitePool) -> BTreeMap<&'static str, Vec<String>> {
        let mut rows = BTreeMap::new();
        for (table, columns) in [
            ("host_identity", "only_row, host_id"),
            ("accepted_document", "only_row, document, digest"),
            ("slot_cursor", "only_row, cursor"),
            ("slots", "app_id, slot"),
            ("instances", "app_id, record, deployment_id, state"),
            ("activity", "app_id, last_active_at_ms"),
            ("meters", "app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds"),
            ("deleted_volumes", "volume_id, report"),
        ] {
            let query = format!("select json_array({columns}) from {table} order by 1");
            rows.insert(table, sqlx::query_scalar(&query).fetch_all(pool).await.unwrap());
        }
        rows
    }

    #[tokio::test]
    async fn upgrading_an_existing_host_preserves_every_table_and_backfills_independent_owners() {
        let directory = tempfile::tempdir().unwrap();
        let migrations = directory.path().join("migrations");
        std::fs::create_dir(&migrations).unwrap();
        std::fs::write(
            migrations.join("0001_host_state.sql"),
            include_str!("../../migrations/0001_host_state.sql"),
        )
        .unwrap();
        let path = directory.path().join("state.db");
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::migrate::Migrator::new(migrations.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "insert into host_identity values (0, 'host-1');
             insert into accepted_document values (0, '{}', 'digest');
             insert into slot_cursor values (0, 9);
             insert into slots values ('slot-owner', 7);
             insert into instances (app_id, record) values ('instance-owner', '{\"deploymentId\":\"deploy-1\",\"state\":\"running\"}');
             insert into activity values ('activity-owner', 123);
             insert into meters values ('meter-owner', 1, 2, 3, 4, 5, 6, 7);
             insert into deleted_volumes values ('vol-1', '{\"appId\":\"deleted-owner\"}');
             insert into deleted_volumes values ('vol-unreadable', '{}');
             insert into deleted_volumes values ('vol-malformed', 'not json');"
        ).execute(&pool).await.unwrap();
        let before = persisted_rows(&pool).await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for table in ["slots", "instances", "activity", "meters", "deleted_volumes"] {
            let legacy_count: i64 =
                sqlx::query_scalar(&format!("select count(*) from {table}_before_foreign_keys"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(legacy_count, i64::try_from(before[table].len()).unwrap());
            let current_count: i64 = sqlx::query_scalar(&format!("select count(*) from {table}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(current_count, 0);
        }
        let identities: i64 = sqlx::query_scalar("select count(*) from apps")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(identities, 0);
        pool.close().await;

        let upgraded = open(&path).await.unwrap();
        assert_eq!(persisted_rows(&upgraded).await, before);
        let owners: Vec<String> = sqlx::query_scalar("select app_id from apps order by app_id")
            .fetch_all(&upgraded)
            .await
            .unwrap();
        assert_eq!(
            owners,
            [
                "activity-owner",
                "deleted-owner",
                "instance-owner",
                "meter-owner",
                "slot-owner"
            ]
        );
        assert!(sqlx::query("pragma foreign_key_check")
            .fetch_all(&upgraded)
            .await
            .unwrap()
            .is_empty());
        let versions: Vec<i64> = sqlx::query_scalar("select version from _sqlx_migrations order by version")
            .fetch_all(&upgraded)
            .await
            .unwrap();
        assert_eq!(&versions[..2], &[1, 2]);
        upgraded.close().await;
        assert_eq!(persisted_rows(&open(&path).await.unwrap()).await, before);
    }

    #[tokio::test]
    async fn every_app_owned_table_rejects_orphans_and_protects_its_parent() {
        let pool = in_memory().await;
        for insert in [
            "insert into slots (app_id, slot) values (?, 0)",
            "insert into instances (app_id, record) values (?, '{}')",
            "insert into activity (app_id, last_active_at_ms) values (?, 1)",
            "insert into meters values (?, 1, 2, 3, 4, 5, 6, 7)",
            "insert into deleted_volumes (volume_id, report) values ('vol-1', json_object('appId', ?))",
        ] {
            let mut tx = pool.begin().await.unwrap();
            let orphan = sqlx::query(insert)
                .bind("app-1")
                .execute(&mut *tx)
                .await
                .unwrap_err();
            assert!(
                orphan.as_database_error().unwrap().is_foreign_key_violation(),
                "{insert}: {orphan}"
            );
            super::ensure_app(&mut tx, "app-1").await.unwrap();
            sqlx::query(insert).bind("app-1").execute(&mut *tx).await.unwrap();
            for mutation in [
                "delete from apps where app_id = 'app-1'",
                "update apps set app_id = 'app-2' where app_id = 'app-1'",
            ] {
                let error = sqlx::query(mutation).execute(&mut *tx).await.unwrap_err();
                assert_eq!(
                    error.as_database_error().unwrap().message(),
                    "FOREIGN KEY constraint failed",
                    "{insert}: {error}"
                );
            }
            tx.rollback().await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_volume_deletion_remains_reported_after_the_app_releases_its_slot_and_instance() {
        let pool = in_memory().await;
        let repositories = Repositories::sqlite(pool.clone());
        repositories
            .slots
            .replace_all(&BTreeMap::from([(app_id(), 0)]), 1)
            .await
            .unwrap();
        repositories
            .instances
            .replace_all(&[instance_record(|_| {})])
            .await
            .unwrap();
        let deletions = BTreeMap::from([(
            volume_id(),
            reported_volume(|report| report.state = protocol::VolumeState::Deleted),
        )]);
        repositories
            .deleted_volumes
            .replace_all(&deletions)
            .await
            .unwrap();

        repositories.slots.replace_all(&BTreeMap::new(), 1).await.unwrap();
        repositories.instances.replace_all(&[]).await.unwrap();
        assert_eq!(repositories.deleted_volumes.all().await.unwrap(), deletions);
        assert!(sqlx::query("delete from apps where app_id = ?")
            .bind(app_id().as_str())
            .execute(&pool)
            .await
            .is_err());
        repositories
            .deleted_volumes
            .replace_all(&BTreeMap::new())
            .await
            .unwrap();
        sqlx::query("delete from apps where app_id = ?")
            .bind(app_id().as_str())
            .execute(&pool)
            .await
            .unwrap();
    }
}
