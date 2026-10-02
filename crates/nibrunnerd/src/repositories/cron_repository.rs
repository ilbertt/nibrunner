use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{
    AppId, CronCommand, CronJobDefinition, CronSchedule, CronTable, Crontab, DeploymentId, TenantEnvironment,
    TenantValue, MAX_CRON_ENVIRONMENT_VARIABLES,
};
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
        let mut transaction = self.pool.begin().await.map_err(StoreError::read)?;
        let tables = sqlx::query!("select app_id, deployment_id, crontab from cron_tables order by app_id")
            .fetch_all(&mut *transaction)
            .await
            .map_err(StoreError::read)?;
        let job_rows = sqlx::query!(
            "select app_id, job_index, schedule, command, has_environment from cron_jobs
             order by app_id, job_index"
        )
        .fetch_all(&mut *transaction)
        .await
        .map_err(StoreError::read)?;
        let environment_rows =
            sqlx::query!("select app_id, job_index, name, value from cron_job_environment")
                .fetch_all(&mut *transaction)
                .await
                .map_err(StoreError::read)?;
        transaction.commit().await.map_err(StoreError::read)?;

        let mut environments: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        for row in environment_rows {
            let value = TenantValue::parse(row.value).map_err(|_| invalid_registration())?;
            environments
                .entry((row.app_id, row.job_index))
                .or_default()
                .insert(row.name, value);
        }
        let mut jobs: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for row in job_rows {
            let variables = environments
                .remove(&(row.app_id.clone(), row.job_index))
                .unwrap_or_default();
            let environment = match row.has_environment {
                0 if variables.is_empty() => None,
                1 if variables.len() <= MAX_CRON_ENVIRONMENT_VARIABLES => {
                    Some(TenantEnvironment::try_from(variables).map_err(|_| invalid_registration())?)
                }
                _ => return Err(invalid_registration()),
            };
            let definitions = jobs.entry(row.app_id).or_default();
            if usize::try_from(row.job_index).ok() != Some(definitions.len()) {
                return Err(invalid_registration());
            }
            definitions.push(CronJobDefinition {
                schedule: CronSchedule::parse(row.schedule).map_err(|_| invalid_registration())?,
                command: CronCommand::parse(row.command).map_err(|_| invalid_registration())?,
                environment,
            });
        }
        let tables = tables
            .into_iter()
            .map(|row| {
                let definitions = jobs.remove(&row.app_id).unwrap_or_default();
                Ok(CronTable {
                    app_id: AppId::parse(row.app_id).map_err(|_| invalid_registration())?,
                    deployment_id: DeploymentId::parse(row.deployment_id)
                        .map_err(|_| invalid_registration())?,
                    jobs: definitions.into(),
                    crontab: row
                        .crontab
                        .map(Crontab::parse)
                        .transpose()
                        .map_err(|_| invalid_registration())?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        if !jobs.is_empty() || !environments.is_empty() {
            return Err(invalid_registration());
        }
        Ok(tables)
    }

    async fn replace_all(&self, tables: &[CronTable]) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(StoreError::write)?;
        sqlx::query!("delete from cron_tables")
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::write)?;
        for table in tables {
            let app_id = table.app_id.as_str();
            let deployment_id = table.deployment_id.as_str();
            let crontab = table.crontab.as_ref().map(Crontab::expose);
            sqlx::query!(
                "insert into cron_tables (app_id, deployment_id, crontab) values (?, ?, ?)",
                app_id,
                deployment_id,
                crontab
            )
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::write)?;
            for (job_index, job) in table.jobs.iter().enumerate() {
                let job_index = i64::try_from(job_index)
                    .map_err(|_| StoreError::Unwritable("a cron job index is too large".to_owned()))?;
                let schedule = job.schedule.as_str();
                let command = job.command.expose();
                let has_environment = job.environment.is_some();
                sqlx::query!(
                    "insert into cron_jobs (app_id, job_index, schedule, command, has_environment)
                     values (?, ?, ?, ?, ?)",
                    app_id,
                    job_index,
                    schedule,
                    command,
                    has_environment
                )
                .execute(&mut *transaction)
                .await
                .map_err(StoreError::write)?;
                if let Some(environment) = &job.environment {
                    for (name, value) in environment.iter() {
                        let value = value.expose();
                        sqlx::query!(
                            "insert into cron_job_environment (app_id, job_index, name, value)
                             values (?, ?, ?, ?)",
                            app_id,
                            job_index,
                            name,
                            value
                        )
                        .execute(&mut *transaction)
                        .await
                        .map_err(StoreError::write)?;
                    }
                }
            }
        }
        transaction.commit().await.map_err(StoreError::write)?;
        Ok(())
    }
}

fn invalid_registration() -> StoreError {
    StoreError::Unreadable("a cron registration contains invalid stored fields".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::store::{in_memory, open};

    fn table(app: &str, deployment: &str) -> CronTable {
        CronTable {
            app_id: AppId::parse(app).unwrap(),
            deployment_id: DeploymentId::parse(deployment).unwrap(),
            jobs: vec![
                CronJobDefinition {
                    schedule: CronSchedule::parse("*/5 * * * *").unwrap(),
                    command: CronCommand::parse("printf '%s' \"$TOKEN\"").unwrap(),
                    environment: Some(
                        BTreeMap::from([
                            ("TOKEN".to_owned(), TenantValue::parse("tenant-secret").unwrap()),
                            ("MESSAGE".to_owned(), TenantValue::parse("hello\n世界").unwrap()),
                        ])
                        .try_into()
                        .unwrap(),
                    ),
                },
                CronJobDefinition {
                    schedule: CronSchedule::parse("0 0 * * *").unwrap(),
                    command: CronCommand::parse("echo midnight").unwrap(),
                    environment: None,
                },
                CronJobDefinition {
                    schedule: CronSchedule::parse("0 0 * * *").unwrap(),
                    command: CronCommand::parse("echo midnight").unwrap(),
                    environment: Some(TenantEnvironment::default()),
                },
            ]
            .into(),
            crontab: Some(Crontab::parse("# tenant-secret\n").unwrap()),
        }
    }

    #[tokio::test]
    async fn a_reopened_repository_restores_the_original_table() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let pool = open(&path).await.unwrap();
        let before = SqliteCron::new(pool.clone());
        let held = vec![table("app-1", "dep-1")];
        before.replace_all(&held).await.unwrap();
        pool.close().await;
        assert_eq!(
            SqliteCron::new(open(&path).await.unwrap()).all().await.unwrap(),
            held
        );
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
        let repository = SqliteCron::new(pool.clone());
        for corrupt in [
            "update cron_jobs set command = 'tenant-secret' || char(10) where job_index = 0",
            "update cron_job_environment set name = 'tenant-secret' where name = 'TOKEN'",
        ] {
            repository.replace_all(&[table("app-1", "dep-1")]).await.unwrap();
            sqlx::query(corrupt).execute(&pool).await.unwrap();
            let error = repository.all().await.unwrap_err();
            assert!(!format!("{error:?} {error}").contains("tenant-secret"));
        }
    }

    #[tokio::test]
    async fn replacing_a_deployment_removes_its_previous_jobs_and_environment() {
        let pool = in_memory().await;
        let repository = SqliteCron::new(pool.clone());
        repository.replace_all(&[table("app-1", "dep-1")]).await.unwrap();
        let empty = CronTable {
            app_id: AppId::parse("app-1").unwrap(),
            deployment_id: DeploymentId::parse("dep-2").unwrap(),
            jobs: Vec::new().into(),
            crontab: None,
        };
        repository
            .replace_all(std::slice::from_ref(&empty))
            .await
            .unwrap();
        assert_eq!(repository.all().await.unwrap(), [empty]);
        for child in ["cron_jobs", "cron_job_environment"] {
            let count: i64 = sqlx::query_scalar(&format!("select count(*) from {child}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[tokio::test]
    async fn every_connection_enforces_cron_ownership_for_disk_and_memory_databases() {
        let directory = tempfile::tempdir().unwrap();
        for pool in [
            open(&directory.path().join("state.db")).await.unwrap(),
            in_memory().await,
        ] {
            SqliteCron::new(pool.clone())
                .replace_all(&[table("app-1", "dep-1")])
                .await
                .unwrap();
            let mut connections = Vec::new();
            for _ in 0..4 {
                connections.push(pool.acquire().await.unwrap());
            }
            for connection in &mut connections {
                let enabled: i64 = sqlx::query_scalar("pragma foreign_keys")
                    .fetch_one(&mut **connection)
                    .await
                    .unwrap();
                assert_eq!(enabled, 1);
                for orphan in [
                    "insert into cron_jobs (app_id, job_index, schedule, command, has_environment)
                     values ('app-2', 0, '* * * * *', 'true', 0)",
                    "insert into cron_job_environment (app_id, job_index, name, value)
                     values ('app-1', 99, 'TOKEN', 'tenant-secret')",
                    "insert into cron_job_environment (app_id, job_index, name, value)
                     values ('app-2', 0, 'TOKEN', 'tenant-secret')",
                ] {
                    let error = sqlx::query(orphan).execute(&mut **connection).await.unwrap_err();
                    assert!(error.as_database_error().unwrap().is_foreign_key_violation());
                }
            }
        }
    }
}
