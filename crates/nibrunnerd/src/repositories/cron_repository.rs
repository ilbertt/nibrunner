use async_trait::async_trait;
use protocol::CronTable;
use sqlx::SqlitePool;

use crate::domain::store::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait CronRepository: Send + Sync {
    async fn all(&self) -> Result<Vec<CronTable>, StoreError>;
    async fn replace_all(&self, tables: &[CronTable]) -> Result<(), StoreError>;
}

pub struct SqliteCron {
    pool: SqlitePool,
}

impl SqliteCron {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CronRepository for SqliteCron {
    async fn all(&self) -> Result<Vec<CronTable>, StoreError> {
        let rows = sqlx::query!("select app_id, record from cron_tables order by app_id")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        rows.into_iter()
            .map(|row| {
                let table: CronTable = serde_json::from_str(&row.record).map_err(|_| {
                    StoreError::Unreadable("a cron registry record could not be decoded".to_owned())
                })?;
                if table.app_id.as_str() != row.app_id {
                    return Err(StoreError::Unreadable(
                        "a cron registry record belongs to another app".to_owned(),
                    ));
                }
                Ok(table)
            })
            .collect()
    }

    async fn replace_all(&self, tables: &[CronTable]) -> Result<(), StoreError> {
        let records = tables
            .iter()
            .map(|table| {
                let record = serde_json::to_string(table).map_err(|_| {
                    StoreError::Unwritable("a cron registry record could not be encoded".to_owned())
                })?;
                Ok((table.app_id.as_str(), record))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let mut transaction = self.pool.begin().await.map_err(StoreError::write)?;
        sqlx::query!("delete from cron_tables")
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::write)?;
        for (app_id, record) in records {
            sqlx::query!(
                "insert into cron_tables (app_id, record) values (?, ?)",
                app_id,
                record
            )
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::write)?;
        }
        transaction.commit().await.map_err(StoreError::write)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::store::in_memory;
    use protocol::{AppId, CronJobDefinitions, Crontab, DeploymentId};

    fn table(app: &str, deployment: &str) -> CronTable {
        CronTable {
            app_id: AppId::parse(app).unwrap(),
            deployment_id: DeploymentId::parse(deployment).unwrap(),
            jobs: CronJobDefinitions::default(),
            crontab: Some(Crontab::parse("# tenant-secret\n").unwrap()),
        }
    }

    #[tokio::test]
    async fn a_reopened_repository_restores_the_original_table() {
        let pool = in_memory().await;
        let before = SqliteCron::new(pool.clone());
        let held = vec![table("app-1", "dep-1")];
        before.replace_all(&held).await.unwrap();
        assert_eq!(SqliteCron::new(pool).all().await.unwrap(), held);
    }

    #[tokio::test]
    async fn a_failed_transaction_does_not_remove_the_previous_registration() {
        let pool = in_memory().await;
        let repository = SqliteCron::new(pool);
        let held = table("app-1", "dep-1");
        repository.replace_all(std::slice::from_ref(&held)).await.unwrap();
        let duplicate = table("app-2", "dep-2");
        assert!(repository
            .replace_all(&[duplicate.clone(), duplicate])
            .await
            .is_err());
        assert_eq!(repository.all().await.unwrap(), vec![held]);
    }

    #[tokio::test]
    async fn replacing_the_registry_removes_apps_no_longer_desired() {
        let repository = SqliteCron::new(in_memory().await);
        repository
            .replace_all(&[table("app-1", "dep-1"), table("app-2", "dep-2")])
            .await
            .unwrap();
        let remaining = table("app-2", "dep-2");
        repository
            .replace_all(std::slice::from_ref(&remaining))
            .await
            .unwrap();
        assert_eq!(repository.all().await.unwrap(), vec![remaining]);
    }

    #[tokio::test]
    async fn corrupt_records_are_reported_without_revealing_their_contents() {
        let pool = in_memory().await;
        sqlx::query("insert into cron_tables(app_id,record) values ('app-1','tenant-secret')")
            .execute(&pool)
            .await
            .unwrap();
        let error = SqliteCron::new(pool).all().await.unwrap_err();
        assert!(!format!("{error:?} {error}").contains("tenant-secret"));
    }
}
