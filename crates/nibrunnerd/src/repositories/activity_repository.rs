use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{AppId, MIN_IDLE_TIMEOUT_MS};
use sqlx::SqlitePool;

use crate::domain::store::StoreError;
use crate::repositories::last_written::LastWritten;

/// How far a reading moves from the row on disk before it is worth a write. The row is a hint:
/// after a restart it seeds when each app was last asked for, so the quiet ones sleep when they
/// are due rather than all together one timeout later, and the shortest timeout it feeds is
/// `MIN_IDLE_TIMEOUT_MS`. So an app taking steady traffic costs a write every half minute rather
/// than every pass, and a quiet host none. What it costs: an app that is quiet when the daemon
/// comes back may sleep up to half a minute before its timeout says. Half and not the whole,
/// because the first reading of the counters after a restart only sets the baseline the second
/// is measured from, so a busy app is credited again two readings in — and a row a whole minute
/// stale would have had the first sleep pass judge its app quiet before that.
const WRITE_TOLERANCE_MS: u64 = MIN_IDLE_TIMEOUT_MS / 2;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait ActivityRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<AppId, i64>, StoreError>;
    /// Leaves the table holding the apps of `activity` and nothing else, each with a reading
    /// within `WRITE_TOLERANCE_MS` of what it was handed: one that moved less than that from its
    /// row is not written.
    async fn replace_all(&self, activity: &BTreeMap<AppId, i64>) -> Result<(), StoreError>;
}

pub struct SqliteActivity {
    pool: SqlitePool,
    last_written: LastWritten<BTreeMap<String, i64>>,
}

impl SqliteActivity {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            last_written: LastWritten::unknown(),
        }
    }

    async fn stored(&self) -> Result<BTreeMap<String, i64>, StoreError> {
        let rows = sqlx::query!("select app_id, last_active_at_ms from activity")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .map(|row| (row.app_id, row.last_active_at_ms))
            .collect())
    }
}

#[async_trait]
impl ActivityRepository for SqliteActivity {
    async fn all(&self) -> Result<BTreeMap<AppId, i64>, StoreError> {
        Ok(self
            .stored()
            .await?
            .into_iter()
            .filter_map(|(app_id, at_ms)| Some((AppId::parse(app_id).ok()?, at_ms)))
            .collect())
    }

    async fn replace_all(&self, activity: &BTreeMap<AppId, i64>) -> Result<(), StoreError> {
        let wanted = activity
            .iter()
            .map(|(app_id, at_ms)| (app_id.as_str().to_owned(), *at_ms))
            .collect();
        let mut delta = self.last_written.towards(wanted, self.stored()).await?;
        delta.keep_unless(|held, wanted| wanted.abs_diff(*held) >= WRITE_TOLERANCE_MS);
        if !delta.is_empty() {
            let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
            for (app_id, at_ms) in delta.changed() {
                sqlx::query!(
                    "insert into activity (app_id, last_active_at_ms) values (?, ?)
                     on conflict (app_id) do update set last_active_at_ms = excluded.last_active_at_ms",
                    app_id,
                    at_ms
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::write)?;
            }
            for app_id in delta.gone() {
                sqlx::query!("delete from activity where app_id = ?", app_id)
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

    const TOLERANCE: i64 = WRITE_TOLERANCE_MS as i64;

    fn app(index: u32) -> AppId {
        AppId::parse(format!("app-{index}")).expect("a fixture is a valid app id")
    }

    async fn repository() -> SqliteActivity {
        SqliteActivity::new(in_memory().await)
    }

    async fn watched() -> (SqlitePool, SqliteActivity) {
        let pool = in_memory().await;
        written::watch(&pool, "activity", "app_id").await;
        (pool.clone(), SqliteActivity::new(pool))
    }

    #[tokio::test]
    async fn what_was_written_is_what_comes_back() {
        let activity = repository().await;
        let held = BTreeMap::from([(app_id(), 1_754_215_200_000)]);
        activity.replace_all(&held).await.unwrap();
        assert_eq!(activity.all().await.unwrap(), held);
    }

    #[tokio::test]
    async fn a_host_that_has_seen_nothing_reads_back_nothing() {
        assert!(repository().await.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_app_this_host_no_longer_runs_stops_being_counted_as_active() {
        let activity = repository().await;
        activity
            .replace_all(&BTreeMap::from([(app_id(), 1)]))
            .await
            .unwrap();
        activity.replace_all(&BTreeMap::new()).await.unwrap();
        assert!(activity.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_later_reading_replaces_the_one_before_it() {
        let activity = repository().await;
        activity
            .replace_all(&BTreeMap::from([(app_id(), 1)]))
            .await
            .unwrap();
        activity
            .replace_all(&BTreeMap::from([(app_id(), 1 + TOLERANCE)]))
            .await
            .unwrap();
        assert_eq!(
            activity.all().await.unwrap().get(&app_id()),
            Some(&(1 + TOLERANCE))
        );
    }

    #[tokio::test]
    async fn a_row_no_app_id_names_is_left_out_rather_than_refusing_the_load() {
        let pool = in_memory().await;
        sqlx::query("insert into activity (app_id, last_active_at_ms) values ('not an app id', 1)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(SqliteActivity::new(pool).all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_first_write_puts_down_everything() {
        let (pool, activity) = watched().await;
        activity
            .replace_all(&BTreeMap::from([(app(1), 1), (app(2), 2)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["insert app-1", "insert app-2"]);
    }

    #[tokio::test]
    async fn a_pass_writes_the_readings_that_moved_and_no_others() {
        let (pool, activity) = watched().await;
        activity
            .replace_all(&BTreeMap::from([(app(1), 1), (app(2), 2), (app(3), 3)]))
            .await
            .unwrap();
        written::writes(&pool).await;

        activity
            .replace_all(&BTreeMap::from([
                (app(2), 2 + TOLERANCE),
                (app(3), 3),
                (app(4), 4),
            ]))
            .await
            .unwrap();
        assert_eq!(
            written::writes(&pool).await,
            ["update app-2", "insert app-4", "delete app-1"]
        );
        assert_eq!(
            activity.all().await.unwrap(),
            BTreeMap::from([(app(2), 2 + TOLERANCE), (app(3), 3), (app(4), 4)])
        );
    }

    #[tokio::test]
    async fn a_pass_that_changed_nothing_writes_nothing() {
        let (pool, activity) = watched().await;
        let held = BTreeMap::from([(app(1), 1), (app(2), 2)]);
        activity.replace_all(&held).await.unwrap();
        written::writes(&pool).await;

        activity.replace_all(&held).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn a_restart_diffs_against_what_the_table_holds_rather_than_writing_over_it() {
        let (pool, before) = watched().await;
        before
            .replace_all(&BTreeMap::from([(app(1), 1), (app(2), 2)]))
            .await
            .unwrap();
        written::writes(&pool).await;

        let after = SqliteActivity::new(pool.clone());
        after
            .replace_all(&BTreeMap::from([(app(1), 1), (app(2), 2)]))
            .await
            .unwrap();
        assert!(written::writes(&pool).await.is_empty());

        after
            .replace_all(&BTreeMap::from([(app(1), 1 + TOLERANCE)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["update app-1", "delete app-2"]);
    }

    #[tokio::test]
    async fn a_reading_that_moved_less_than_the_tolerance_is_not_written() {
        let (pool, activity) = watched().await;
        activity
            .replace_all(&BTreeMap::from([(app(1), 1)]))
            .await
            .unwrap();
        written::writes(&pool).await;

        activity
            .replace_all(&BTreeMap::from([(app(1), TOLERANCE)]))
            .await
            .unwrap();
        assert!(written::writes(&pool).await.is_empty());
        assert_eq!(activity.all().await.unwrap().get(&app(1)), Some(&1));
    }

    #[tokio::test]
    async fn a_reading_that_moved_the_tolerance_is_written() {
        let (pool, activity) = watched().await;
        activity
            .replace_all(&BTreeMap::from([(app(1), 1)]))
            .await
            .unwrap();
        written::writes(&pool).await;

        activity
            .replace_all(&BTreeMap::from([(app(1), 1 + TOLERANCE)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["update app-1"]);
        assert_eq!(activity.all().await.unwrap().get(&app(1)), Some(&(1 + TOLERANCE)));
    }

    #[tokio::test]
    async fn an_app_seen_for_the_first_time_is_written_whatever_it_reads() {
        let (pool, activity) = watched().await;
        activity
            .replace_all(&BTreeMap::from([(app(1), 1)]))
            .await
            .unwrap();
        written::writes(&pool).await;

        activity
            .replace_all(&BTreeMap::from([(app(1), 1), (app(2), 2)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["insert app-2"]);
    }

    #[tokio::test]
    async fn an_app_no_longer_held_is_deleted_whatever_the_others_read() {
        let (pool, activity) = watched().await;
        activity
            .replace_all(&BTreeMap::from([(app(1), 1), (app(2), 2)]))
            .await
            .unwrap();
        written::writes(&pool).await;

        activity
            .replace_all(&BTreeMap::from([(app(1), 2)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["delete app-2"]);
        assert_eq!(activity.all().await.unwrap(), BTreeMap::from([(app(1), 1)]));
    }

    #[tokio::test]
    async fn moves_under_the_tolerance_add_up_to_a_write_once_they_cross_it() {
        let (pool, activity) = watched().await;
        activity
            .replace_all(&BTreeMap::from([(app(1), 0)]))
            .await
            .unwrap();
        written::writes(&pool).await;

        for reading in [TOLERANCE / 2, TOLERANCE - 1] {
            activity
                .replace_all(&BTreeMap::from([(app(1), reading)]))
                .await
                .unwrap();
            assert!(written::writes(&pool).await.is_empty());
        }
        activity
            .replace_all(&BTreeMap::from([(app(1), TOLERANCE)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["update app-1"]);
        assert_eq!(activity.all().await.unwrap().get(&app(1)), Some(&TOLERANCE));
    }
}
