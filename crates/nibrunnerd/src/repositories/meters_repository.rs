use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{AppId, UsageMeters};
use sqlx::SqlitePool;

use crate::domain::store::StoreError;
use crate::repositories::last_written::LastWritten;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait MeterRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<AppId, UsageMeters>, StoreError>;
    /// Leaves the table holding `meters` and nothing else, writing only what differs from the
    /// last write.
    async fn replace_all(&self, meters: &BTreeMap<AppId, UsageMeters>) -> Result<(), StoreError>;
}

pub struct SqliteMeters {
    pool: SqlitePool,
    last_written: LastWritten<BTreeMap<String, UsageMeters>>,
}

impl SqliteMeters {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            last_written: LastWritten::unknown(),
        }
    }

    async fn stored(&self) -> Result<BTreeMap<String, UsageMeters>, StoreError> {
        let rows = sqlx::query!("select app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds from meters")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.app_id,
                    UsageMeters {
                        running_ms: as_counted(row.running_ms),
                        idle_ms: as_counted(row.idle_ms),
                        cpu_ms: as_counted(row.cpu_ms),
                        rx_bytes: as_counted(row.rx_bytes),
                        tx_bytes: as_counted(row.tx_bytes),
                        disk_provisioned_mib_seconds: as_counted(row.disk_provisioned_mib_seconds),
                        disk_used_mib_seconds: as_counted(row.disk_used_mib_seconds),
                    },
                )
            })
            .collect())
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
        Ok(self
            .stored()
            .await?
            .into_iter()
            .filter_map(|(app_id, meter)| Some((AppId::parse(app_id).ok()?, meter)))
            .collect())
    }

    async fn replace_all(&self, meters: &BTreeMap<AppId, UsageMeters>) -> Result<(), StoreError> {
        let wanted = meters
            .iter()
            .map(|(app_id, meter)| (app_id.as_str().to_owned(), *meter))
            .collect();
        let delta = self.last_written.towards(wanted, self.stored()).await?;
        if !delta.is_empty() {
            let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
            for (app_id, meter) in delta.changed() {
                let running_ms = as_stored(meter.running_ms);
                let idle_ms = as_stored(meter.idle_ms);
                let cpu_ms = as_stored(meter.cpu_ms);
                let rx_bytes = as_stored(meter.rx_bytes);
                let tx_bytes = as_stored(meter.tx_bytes);
                let disk_provisioned = as_stored(meter.disk_provisioned_mib_seconds);
                let disk_used = as_stored(meter.disk_used_mib_seconds);
                sqlx::query!(
                    "insert into meters (app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds) values (?, ?, ?, ?, ?, ?, ?, ?)
                     on conflict (app_id) do update set
                        running_ms = excluded.running_ms,
                        idle_ms = excluded.idle_ms,
                        cpu_ms = excluded.cpu_ms,
                        rx_bytes = excluded.rx_bytes,
                        tx_bytes = excluded.tx_bytes,
                        disk_provisioned_mib_seconds = excluded.disk_provisioned_mib_seconds,
                        disk_used_mib_seconds = excluded.disk_used_mib_seconds",
                    app_id,
                    running_ms,
                    idle_ms,
                    cpu_ms,
                    rx_bytes,
                    tx_bytes,
                    disk_provisioned,
                    disk_used
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::write)?;
            }
            for app_id in delta.gone() {
                sqlx::query!("delete from meters where app_id = ?", app_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(StoreError::write)?;
            }
            tx.commit().await.map_err(StoreError::write)?;
        }
        delta.written();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::store::in_memory;
    use crate::repositories::last_written::written;
    use crate::test_support::app_id;

    fn app(index: u32) -> AppId {
        AppId::parse(format!("app-{index}")).expect("a fixture is a valid app id")
    }

    async fn repository() -> SqliteMeters {
        SqliteMeters::new(in_memory().await)
    }

    async fn watched() -> (SqlitePool, SqliteMeters) {
        let pool = in_memory().await;
        written::watch(&pool, "meters", "app_id").await;
        (pool.clone(), SqliteMeters::new(pool))
    }

    fn meter() -> UsageMeters {
        UsageMeters {
            running_ms: 3_600_000,
            idle_ms: 900_000,
            cpu_ms: 42_150,
            rx_bytes: 1_073_741_824,
            tx_bytes: 4_294_967_296,
            disk_provisioned_mib_seconds: 29_491_200,
            disk_used_mib_seconds: 5_242_880,
        }
    }

    fn metered(running_ms: u64) -> UsageMeters {
        UsageMeters {
            running_ms,
            ..meter()
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

    #[tokio::test]
    async fn the_first_write_puts_down_everything() {
        let (pool, meters) = watched().await;
        meters
            .replace_all(&BTreeMap::from([(app(1), meter()), (app(2), meter())]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["insert app-1", "insert app-2"]);
    }

    #[tokio::test]
    async fn a_pass_writes_the_meters_that_moved_and_no_others() {
        let (pool, meters) = watched().await;
        meters
            .replace_all(&BTreeMap::from([
                (app(1), metered(1)),
                (app(2), metered(2)),
                (app(3), metered(3)),
            ]))
            .await
            .unwrap();
        written::writes(&pool).await;

        let moved = BTreeMap::from([(app(2), metered(9)), (app(3), metered(3)), (app(4), metered(4))]);
        meters.replace_all(&moved).await.unwrap();
        assert_eq!(
            written::writes(&pool).await,
            ["update app-2", "insert app-4", "delete app-1"]
        );
        assert_eq!(meters.all().await.unwrap(), moved);
    }

    #[tokio::test]
    async fn a_pass_that_changed_nothing_writes_nothing() {
        let (pool, meters) = watched().await;
        let held = BTreeMap::from([(app(1), metered(1)), (app(2), metered(2))]);
        meters.replace_all(&held).await.unwrap();
        written::writes(&pool).await;

        meters.replace_all(&held).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn a_restart_diffs_against_what_the_table_holds_rather_than_writing_over_it() {
        let (pool, before) = watched().await;
        let held = BTreeMap::from([(app(1), metered(1)), (app(2), metered(2))]);
        before.replace_all(&held).await.unwrap();
        written::writes(&pool).await;

        let after = SqliteMeters::new(pool.clone());
        after.replace_all(&held).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());

        after
            .replace_all(&BTreeMap::from([(app(1), metered(5))]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["update app-1", "delete app-2"]);
    }
}
