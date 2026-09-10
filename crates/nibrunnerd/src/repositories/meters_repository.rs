use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{AppId, UsageMeters};
use sqlx::SqlitePool;

use crate::domain::store::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait MeterRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<AppId, UsageMeters>, StoreError>;
    async fn replace_all(&self, meters: &BTreeMap<AppId, UsageMeters>) -> Result<(), StoreError>;
}

pub struct SqliteMeters {
    pool: SqlitePool,
}

impl SqliteMeters {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

// SQLite has no unsigned integer, so what goes in as a count comes back as an `i64` and a host
// that ran long enough to overflow one has bigger news than its bill.
fn as_stored(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn as_counted(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[async_trait]
impl MeterRepository for SqliteMeters {
    async fn all(&self) -> Result<BTreeMap<AppId, UsageMeters>, StoreError> {
        let rows = sqlx::query!("select app_id, running_ms, idle_ms, cpu_ms, rx_bytes from meters")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some((
                    AppId::parse(row.app_id).ok()?,
                    UsageMeters {
                        running_ms: as_counted(row.running_ms),
                        idle_ms: as_counted(row.idle_ms),
                        cpu_ms: as_counted(row.cpu_ms),
                        rx_bytes: as_counted(row.rx_bytes),
                    },
                ))
            })
            .collect())
    }

    async fn replace_all(&self, meters: &BTreeMap<AppId, UsageMeters>) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
        sqlx::query!("delete from meters")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        for (app_id, meter) in meters {
            let app_id = app_id.as_str();
            let running_ms = as_stored(meter.running_ms);
            let idle_ms = as_stored(meter.idle_ms);
            let cpu_ms = as_stored(meter.cpu_ms);
            let rx_bytes = as_stored(meter.rx_bytes);
            sqlx::query!(
                "insert into meters (app_id, running_ms, idle_ms, cpu_ms, rx_bytes) values (?, ?, ?, ?, ?)",
                app_id,
                running_ms,
                idle_ms,
                cpu_ms,
                rx_bytes
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        }
        tx.commit().await.map_err(StoreError::write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::store::in_memory;
    use crate::test_support::app_id;

    async fn repository() -> SqliteMeters {
        SqliteMeters::new(in_memory().await)
    }

    fn meter() -> UsageMeters {
        UsageMeters {
            running_ms: 3_600_000,
            idle_ms: 900_000,
            cpu_ms: 42_150,
            rx_bytes: 1_073_741_824,
        }
    }

    #[tokio::test]
    async fn what_was_written_is_what_comes_back() {
        let meters = repository().await;
        let held = BTreeMap::from([(app_id(), meter())]);
        meters.replace_all(&held).await.unwrap();
        assert_eq!(meters.all().await.unwrap(), held);
    }

    #[tokio::test]
    async fn a_host_that_has_metered_nothing_reads_back_nothing() {
        assert!(repository().await.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_app_left_out_of_a_pass_is_left_out_of_the_table() {
        let meters = repository().await;
        meters
            .replace_all(&BTreeMap::from([(app_id(), meter())]))
            .await
            .unwrap();
        meters.replace_all(&BTreeMap::new()).await.unwrap();
        assert!(meters.all().await.unwrap().is_empty());
    }
}
