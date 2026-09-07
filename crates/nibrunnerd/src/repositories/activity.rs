use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::AppId;
use sqlx::SqlitePool;

use crate::repositories::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait ActivityRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<AppId, i64>, StoreError>;
    async fn replace_all(&self, activity: &BTreeMap<AppId, i64>) -> Result<(), StoreError>;
}

pub struct SqliteActivity {
    pool: SqlitePool,
}

impl SqliteActivity {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ActivityRepository for SqliteActivity {
    async fn all(&self) -> Result<BTreeMap<AppId, i64>, StoreError> {
        let rows = sqlx::query!("select app_id, last_active_at_ms from activity")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| Some((AppId::parse(row.app_id).ok()?, row.last_active_at_ms)))
            .collect())
    }

    async fn replace_all(&self, activity: &BTreeMap<AppId, i64>) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
        sqlx::query!("delete from activity")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        for (app_id, at_ms) in activity {
            let app_id = app_id.as_str();
            sqlx::query!(
                "insert into activity (app_id, last_active_at_ms) values (?, ?)",
                app_id,
                at_ms
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
    use crate::repositories::in_memory;
    use crate::test_support::app_id;

    async fn repository() -> SqliteActivity {
        SqliteActivity::new(in_memory().await)
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
            .replace_all(&BTreeMap::from([(app_id(), 9)]))
            .await
            .unwrap();
        assert_eq!(activity.all().await.unwrap().get(&app_id()), Some(&9));
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
}
