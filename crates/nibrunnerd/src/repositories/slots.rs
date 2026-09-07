use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::AppId;
use sqlx::SqlitePool;

use crate::repositories::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait SlotRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<AppId, u32>, StoreError>;
    async fn cursor(&self) -> Result<i64, StoreError>;
    async fn replace_all(&self, assignments: &BTreeMap<AppId, u32>, cursor: i64) -> Result<(), StoreError>;
}

pub struct SqliteSlots {
    pool: SqlitePool,
}

impl SqliteSlots {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SlotRepository for SqliteSlots {
    async fn all(&self) -> Result<BTreeMap<AppId, u32>, StoreError> {
        let rows = sqlx::query!("select app_id, slot from slots order by slot")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let app_id = AppId::parse(row.app_id).ok()?;
                Some((app_id, u32::try_from(row.slot).ok()?))
            })
            .collect())
    }

    async fn cursor(&self) -> Result<i64, StoreError> {
        let held = sqlx::query!("select cursor from slot_cursor where only_row = 0")
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(held.map_or(0, |row| row.cursor))
    }

    async fn replace_all(&self, assignments: &BTreeMap<AppId, u32>, cursor: i64) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
        sqlx::query!("delete from slots")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        for (app_id, slot) in assignments {
            let app_id = app_id.as_str();
            let slot = i64::from(*slot);
            sqlx::query!("insert into slots (app_id, slot) values (?, ?)", app_id, slot)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::write)?;
        }
        sqlx::query!(
            "insert into slot_cursor (only_row, cursor) values (0, ?)
         on conflict (only_row) do update set cursor = excluded.cursor",
            cursor
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::write)?;
        tx.commit().await.map_err(StoreError::write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::in_memory;

    fn app(index: u32) -> AppId {
        AppId::parse(format!("app-{index}")).expect("a fixture is a valid app id")
    }

    async fn repository() -> SqliteSlots {
        SqliteSlots::new(in_memory().await)
    }

    #[tokio::test]
    async fn what_was_written_is_what_comes_back() {
        let slots = repository().await;
        let held = BTreeMap::from([(app(1), 0), (app(2), 5)]);
        slots.replace_all(&held, 6).await.unwrap();
        assert_eq!(slots.all().await.unwrap(), held);
        assert_eq!(slots.cursor().await.unwrap(), 6);
    }

    #[tokio::test]
    async fn a_slot_that_was_released_does_not_survive_the_next_write() {
        let slots = repository().await;
        slots
            .replace_all(&BTreeMap::from([(app(1), 0), (app(2), 1)]), 2)
            .await
            .unwrap();
        slots
            .replace_all(&BTreeMap::from([(app(2), 1)]), 2)
            .await
            .unwrap();

        let held = slots.all().await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held.get(&app(1)), None);
    }

    #[tokio::test]
    async fn a_host_with_no_cursor_yet_reads_zero_rather_than_failing() {
        assert_eq!(repository().await.cursor().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_cursor_lands_with_the_slots_it_was_measured_against() {
        let slots = repository().await;
        slots
            .replace_all(&BTreeMap::from([(app(1), 0)]), 1)
            .await
            .unwrap();
        slots
            .replace_all(&BTreeMap::from([(app(1), 0), (app(2), 1)]), 2)
            .await
            .unwrap();
        assert_eq!(slots.cursor().await.unwrap(), 2);
        assert_eq!(slots.all().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_slot_no_app_id_names_is_left_out_rather_than_refusing_the_load() {
        let pool = in_memory().await;
        sqlx::query("insert into slots (app_id, slot) values ('not an app id', 3)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(SqliteSlots::new(pool).all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn two_apps_cannot_hold_the_same_slot() {
        let pool = in_memory().await;
        SqliteSlots::new(pool.clone())
            .replace_all(&BTreeMap::from([(app(1), 0)]), 1)
            .await
            .unwrap();
        let clash = sqlx::query("insert into slots (app_id, slot) values ('app-2', 0)")
            .execute(&pool)
            .await;
        assert!(clash.is_err(), "the second app took a slot the first holds");
    }
}
