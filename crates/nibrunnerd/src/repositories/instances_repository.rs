use async_trait::async_trait;
use protocol::AppId;
use sqlx::SqlitePool;

use crate::domain::report::instance_record::InstanceRecord;
use crate::domain::store::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait InstanceRepository: Send + Sync {
    async fn all(&self) -> Result<Vec<InstanceRecord>, StoreError>;
    async fn replace_all(&self, records: &[InstanceRecord]) -> Result<(), StoreError>;
    async fn summary(&self) -> Result<Vec<(AppId, String)>, StoreError>;
}

pub struct SqliteInstances {
    pool: SqlitePool,
}

impl SqliteInstances {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl InstanceRepository for SqliteInstances {
    async fn all(&self) -> Result<Vec<InstanceRecord>, StoreError> {
        let rows = sqlx::query!("select record from instances order by app_id")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| serde_json::from_str(&row.record).ok())
            .collect())
    }

    async fn replace_all(&self, records: &[InstanceRecord]) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
        sqlx::query!("delete from instances")
            .execute(&mut *tx)
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
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        }
        tx.commit().await.map_err(StoreError::write)
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
    use crate::test_support::instance_record;
    use protocol::InstanceState;

    async fn repository() -> SqliteInstances {
        SqliteInstances::new(in_memory().await)
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
}
