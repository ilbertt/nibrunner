use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::AppId;
use sqlx::SqlitePool;

use crate::domain::report::instance_record::InstanceRecord;
use crate::domain::store::StoreError;
use crate::repositories::last_written::LastWritten;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait InstanceRepository: Send + Sync {
    async fn all(&self) -> Result<Vec<InstanceRecord>, StoreError>;
    /// Leaves the table holding `records` and nothing else, writing only what differs from the
    /// last write.
    async fn replace_all(&self, records: &[InstanceRecord]) -> Result<(), StoreError>;
    async fn summary(&self) -> Result<Vec<(AppId, String)>, StoreError>;
}

pub struct SqliteInstances {
    pool: SqlitePool,
    last_written: LastWritten<BTreeMap<String, String>>,
}

impl SqliteInstances {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            last_written: LastWritten::unknown(),
        }
    }

    async fn stored(&self) -> Result<BTreeMap<String, String>, StoreError> {
        let rows = sqlx::query!("select app_id, record from instances")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows.into_iter().map(|row| (row.app_id, row.record)).collect())
    }
}

#[async_trait]
impl InstanceRepository for SqliteInstances {
    async fn all(&self) -> Result<Vec<InstanceRecord>, StoreError> {
        Ok(self
            .stored()
            .await?
            .into_values()
            .filter_map(|record| serde_json::from_str(&record).ok())
            .collect())
    }

    async fn replace_all(&self, records: &[InstanceRecord]) -> Result<(), StoreError> {
        let wanted = records
            .iter()
            .map(|record| {
                let rendered = serde_json::to_string(record)
                    .map_err(|error| StoreError::Unwritable(error.to_string()))?;
                Ok((record.app_id.as_str().to_owned(), rendered))
            })
            .collect::<Result<_, StoreError>>()?;
        let delta = self.last_written.towards(wanted, self.stored()).await?;
        if !delta.is_empty() {
            let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
            for (app_id, record) in delta.changed() {
                sqlx::query!(
                    "insert into instances (app_id, record) values (?, ?)
                     on conflict (app_id) do update set record = excluded.record",
                    app_id,
                    record
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::write)?;
            }
            for app_id in delta.gone() {
                sqlx::query!("delete from instances where app_id = ?", app_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(StoreError::write)?;
            }
            tx.commit().await.map_err(StoreError::write)?;
        }
        delta.written();
        Ok(())
    }

    async fn summary(&self) -> Result<Vec<(AppId, String)>, StoreError> {
        let rows = sqlx::query!("select app_id, state from instances order by app_id")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| Some((AppId::parse(row.app_id).ok()?, row.state?)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::store::in_memory;
    use crate::repositories::last_written::written;
    use crate::test_support::instance_record;
    use protocol::InstanceState;

    async fn repository() -> SqliteInstances {
        SqliteInstances::new(in_memory().await)
    }

    async fn watched() -> (SqlitePool, SqliteInstances) {
        let pool = in_memory().await;
        written::watch(&pool, "instances", "app_id").await;
        (pool.clone(), SqliteInstances::new(pool))
    }

    fn record_of(app_id: &str, state: InstanceState) -> InstanceRecord {
        instance_record(|record| {
            record.app_id = AppId::parse(app_id).unwrap();
            record.state = state;
        })
    }

    #[tokio::test]
    async fn what_was_written_is_what_comes_back() {
        let instances = repository().await;
        let record = instance_record(|_| {});
        instances
            .replace_all(std::slice::from_ref(&record))
            .await
            .unwrap();
        assert_eq!(instances.all().await.unwrap(), vec![record]);
    }

    #[tokio::test]
    async fn a_host_that_has_written_nothing_reads_back_nothing() {
        assert!(repository().await.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_app_this_host_no_longer_runs_does_not_survive_the_next_write() {
        let instances = repository().await;
        let first = instance_record(|_| {});
        let second = instance_record(|record| {
            record.app_id = AppId::parse("app-2").unwrap();
        });
        instances
            .replace_all(&[first.clone(), second.clone()])
            .await
            .unwrap();
        instances
            .replace_all(std::slice::from_ref(&second))
            .await
            .unwrap();
        assert_eq!(instances.all().await.unwrap(), vec![second]);
    }

    #[tokio::test]
    async fn a_record_this_daemon_cannot_read_is_left_out_rather_than_failing_the_load() {
        let pool = in_memory().await;
        sqlx::query("insert into instances (app_id, record) values ('app-9', '{\"nonsense\":true}')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(SqliteInstances::new(pool).all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_state_an_operator_reads_comes_from_the_record_rather_than_a_second_copy() {
        let instances = repository().await;
        instances
            .replace_all(&[instance_record(|record| record.state = InstanceState::Idle)])
            .await
            .unwrap();
        assert_eq!(
            instances.summary().await.unwrap(),
            vec![(crate::test_support::app_id(), "idle".to_string())]
        );
    }

    #[tokio::test]
    async fn writing_nothing_empties_the_table() {
        let instances = repository().await;
        instances.replace_all(&[instance_record(|_| {})]).await.unwrap();
        instances.replace_all(&[]).await.unwrap();
        assert!(instances.all().await.unwrap().is_empty());
        assert!(instances.summary().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_first_write_puts_down_everything() {
        let (pool, instances) = watched().await;
        instances
            .replace_all(&[
                record_of("app-1", InstanceState::Running),
                record_of("app-2", InstanceState::Idle),
            ])
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["insert app-1", "insert app-2"]);
    }

    #[tokio::test]
    async fn a_pass_writes_the_records_it_changed_and_no_others() {
        let (pool, instances) = watched().await;
        instances
            .replace_all(&[
                record_of("app-1", InstanceState::Running),
                record_of("app-2", InstanceState::Running),
                record_of("app-3", InstanceState::Running),
            ])
            .await
            .unwrap();
        written::writes(&pool).await;

        instances
            .replace_all(&[
                record_of("app-2", InstanceState::Idle),
                record_of("app-3", InstanceState::Running),
                record_of("app-4", InstanceState::Running),
            ])
            .await
            .unwrap();
        assert_eq!(
            written::writes(&pool).await,
            ["update app-2", "insert app-4", "delete app-1"]
        );
        assert_eq!(
            instances.summary().await.unwrap(),
            vec![
                (AppId::parse("app-2").unwrap(), "idle".to_string()),
                (AppId::parse("app-3").unwrap(), "running".to_string()),
                (AppId::parse("app-4").unwrap(), "running".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_pass_that_changed_nothing_writes_nothing() {
        let (pool, instances) = watched().await;
        let held = [record_of("app-1", InstanceState::Running)];
        instances.replace_all(&held).await.unwrap();
        written::writes(&pool).await;

        instances.replace_all(&held).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn a_restart_diffs_against_what_the_table_holds_rather_than_writing_over_it() {
        let (pool, before) = watched().await;
        let held = [record_of("app-1", InstanceState::Running)];
        before.replace_all(&held).await.unwrap();
        written::writes(&pool).await;

        let after = SqliteInstances::new(pool.clone());
        after.replace_all(&held).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());

        after
            .replace_all(&[record_of("app-1", InstanceState::Idle)])
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["update app-1"]);
    }
}
