use sqlx::SqlitePool;

use super::StoreError;

pub(super) async fn finish_foreign_key_upgrade(pool: &SqlitePool) -> Result<(), StoreError> {
    let mut transaction = pool
        .begin_with("begin immediate")
        .await
        .map_err(StoreError::write)?;
    let has_legacy_tables: bool = sqlx::query_scalar(
        "select exists (select 1 from sqlite_schema where type = 'table' and name = 'slots_before_foreign_keys')",
    )
    .fetch_one(&mut *transaction)
    .await
    .map_err(StoreError::read)?;
    if has_legacy_tables {
        sqlx::raw_sql(
            r#"insert into apps (app_id)
select app_id from slots_before_foreign_keys
union select app_id from instances_before_foreign_keys
union select app_id from activity_before_foreign_keys
union select app_id from meters_before_foreign_keys
union select case when json_valid(report) then json_extract(report, '$.appId') end
    from deleted_volumes_before_foreign_keys
    where case when json_valid(report) then json_extract(report, '$.appId') end is not null
on conflict (app_id) do nothing;

insert into slots (app_id, slot)
select app_id, slot from slots_before_foreign_keys;
insert into instances (app_id, record)
select app_id, record from instances_before_foreign_keys;
insert into activity (app_id, last_active_at_ms)
select app_id, last_active_at_ms from activity_before_foreign_keys;
insert into meters (app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds)
select app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds from meters_before_foreign_keys;
insert into deleted_volumes (volume_id, report)
select volume_id, report from deleted_volumes_before_foreign_keys;
drop table slots_before_foreign_keys;
drop table instances_before_foreign_keys;
drop table activity_before_foreign_keys;
drop table meters_before_foreign_keys;
drop table deleted_volumes_before_foreign_keys;"#,
        )
        .execute(&mut *transaction)
        .await
        .map_err(StoreError::write)?;
    }
    transaction.commit().await.map_err(StoreError::write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn a_failed_data_upgrade_preserves_the_source_and_retries_without_partial_copies() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../../migrations/0001_host_state.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "insert into slots values ('app-1', 3);
             insert into activity values ('app-2', 123);",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!("../../../migrations/0002_foreign_keys.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "create trigger reject_activity before insert on activity
             begin select raise(abort, 'upgrade interrupted'); end;",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(finish_foreign_key_upgrade(&pool).await.is_err());
        for table in ["apps", "slots", "activity"] {
            let rows: i64 = sqlx::query_scalar(&format!("select count(*) from {table}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(rows, 0);
        }
        for table in ["slots", "activity"] {
            let rows: i64 = sqlx::query_scalar(&format!("select count(*) from {table}_before_foreign_keys"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(rows, 1);
        }
        sqlx::raw_sql("drop trigger reject_activity")
            .execute(&pool)
            .await
            .unwrap();
        finish_foreign_key_upgrade(&pool).await.unwrap();
        finish_foreign_key_upgrade(&pool).await.unwrap();
        let slot: i64 = sqlx::query_scalar("select slot from slots where app_id = 'app-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(slot, 3);
        let at_ms: i64 = sqlx::query_scalar("select last_active_at_ms from activity where app_id = 'app-2'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(at_ms, 123);
        let legacy_tables: i64 =
            sqlx::query_scalar("select count(*) from sqlite_schema where name like '%_before_foreign_keys'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(legacy_tables, 0);
    }
}
