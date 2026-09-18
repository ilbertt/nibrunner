use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::AppId;
use sqlx::SqlitePool;

use crate::domain::store::StoreError;
use crate::repositories::last_written::LastWritten;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait SlotRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<AppId, u32>, StoreError>;
    async fn cursor(&self) -> Result<i64, StoreError>;
    /// Leaves the table holding `assignments` and nothing else, writing only what differs from
    /// the last write.
    async fn replace_all(&self, assignments: &BTreeMap<AppId, u32>, cursor: i64) -> Result<(), StoreError>;
}

pub struct SqliteSlots {
    pool: SqlitePool,
    last_written: LastWritten<BTreeMap<String, i64>>,
    last_cursor: LastWritten<i64>,
}

impl SqliteSlots {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            last_written: LastWritten::unknown(),
            last_cursor: LastWritten::unknown(),
        }
    }

    async fn stored(&self) -> Result<BTreeMap<String, i64>, StoreError> {
        let rows = sqlx::query!("select app_id, slot from slots")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows.into_iter().map(|row| (row.app_id, row.slot)).collect())
    }
}

#[async_trait]
impl SlotRepository for SqliteSlots {
    async fn all(&self) -> Result<BTreeMap<AppId, u32>, StoreError> {
        Ok(self
            .stored()
            .await?
            .into_iter()
            .filter_map(|(app_id, slot)| {
                let app_id = AppId::parse(app_id).ok()?;
                Some((app_id, u32::try_from(slot).ok()?))
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
        let wanted = assignments
            .iter()
            .map(|(app_id, slot)| (app_id.as_str().to_owned(), i64::from(*slot)))
            .collect();
        let slots = self.last_written.towards(wanted, self.stored()).await?;
        let cursor = self.last_cursor.towards(cursor, self.cursor()).await?;
        if !slots.is_empty() || !cursor.is_empty() {
            let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
            // Released before taken: a slot let go of and reissued in the same pass would
            // otherwise meet the old row's `unique` on its way in.
            for app_id in slots.gone() {
                sqlx::query!("delete from slots where app_id = ?", app_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(StoreError::write)?;
            }
            for (app_id, slot) in slots.changed() {
                sqlx::query!(
                    "insert into slots (app_id, slot) values (?, ?)
                     on conflict (app_id) do update set slot = excluded.slot",
                    app_id,
                    slot
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::write)?;
            }
            if !cursor.is_empty() {
                sqlx::query!(
                    "insert into slot_cursor (only_row, cursor) values (0, ?)
                     on conflict (only_row) do update set cursor = excluded.cursor",
                    cursor.wanted
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::write)?;
            }
            tx.commit().await.map_err(StoreError::write)?;
        }
        slots.written();
        cursor.written();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::store::in_memory;
    use crate::repositories::last_written::written;

    fn app(index: u32) -> AppId {
        AppId::parse(format!("app-{index}")).expect("a fixture is a valid app id")
    }

    async fn repository() -> SqliteSlots {
        SqliteSlots::new(in_memory().await)
    }

    async fn watched() -> (SqlitePool, SqliteSlots) {
        let pool = in_memory().await;
        written::watch(&pool, "slots", "app_id").await;
        written::watch(&pool, "slot_cursor", "only_row").await;
        (pool.clone(), SqliteSlots::new(pool))
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

    #[tokio::test]
    async fn the_first_write_puts_down_everything() {
        let (pool, slots) = watched().await;
        slots
            .replace_all(&BTreeMap::from([(app(1), 0), (app(2), 1)]), 2)
            .await
            .unwrap();
        assert_eq!(
            written::writes(&pool).await,
            ["insert app-1", "insert app-2", "insert 0"]
        );
    }

    #[tokio::test]
    async fn a_pass_writes_the_rows_it_changed_and_no_others() {
        let (pool, slots) = watched().await;
        slots
            .replace_all(&BTreeMap::from([(app(1), 0), (app(2), 1), (app(3), 2)]), 3)
            .await
            .unwrap();
        written::writes(&pool).await;

        slots
            .replace_all(&BTreeMap::from([(app(2), 1), (app(3), 2), (app(4), 3)]), 4)
            .await
            .unwrap();
        assert_eq!(
            written::writes(&pool).await,
            ["delete app-1", "insert app-4", "update 0"]
        );
        assert_eq!(
            slots.all().await.unwrap(),
            BTreeMap::from([(app(2), 1), (app(3), 2), (app(4), 3)])
        );
    }

    #[tokio::test]
    async fn a_pass_that_changed_nothing_writes_nothing() {
        let (pool, slots) = watched().await;
        let held = BTreeMap::from([(app(1), 0), (app(2), 1)]);
        slots.replace_all(&held, 2).await.unwrap();
        written::writes(&pool).await;

        slots.replace_all(&held, 2).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn the_cursor_is_written_only_when_it_moved() {
        let (pool, slots) = watched().await;
        let held = BTreeMap::from([(app(1), 0)]);
        slots.replace_all(&held, 1).await.unwrap();
        written::writes(&pool).await;

        slots.replace_all(&held, 5).await.unwrap();
        assert_eq!(written::writes(&pool).await, ["update 0"]);
        assert_eq!(slots.cursor().await.unwrap(), 5);
    }

    #[tokio::test]
    async fn a_restart_diffs_against_what_the_table_holds_rather_than_writing_over_it() {
        let (pool, before) = watched().await;
        before
            .replace_all(&BTreeMap::from([(app(1), 0), (app(2), 1)]), 2)
            .await
            .unwrap();
        written::writes(&pool).await;

        let after = SqliteSlots::new(pool.clone());
        after
            .replace_all(&BTreeMap::from([(app(1), 0), (app(2), 1)]), 2)
            .await
            .unwrap();
        assert!(written::writes(&pool).await.is_empty());

        after
            .replace_all(&BTreeMap::from([(app(2), 1), (app(3), 2)]), 3)
            .await
            .unwrap();
        assert_eq!(
            written::writes(&pool).await,
            ["delete app-1", "insert app-3", "update 0"]
        );
    }

    #[tokio::test]
    async fn a_slot_released_and_reissued_in_the_same_pass_lands() {
        let slots = repository().await;
        slots
            .replace_all(&BTreeMap::from([(app(1), 0)]), 1)
            .await
            .unwrap();
        slots
            .replace_all(&BTreeMap::from([(app(2), 0)]), 1)
            .await
            .unwrap();
        assert_eq!(slots.all().await.unwrap(), BTreeMap::from([(app(2), 0)]));
    }

    #[tokio::test]
    async fn only_a_write_that_landed_is_remembered_as_the_last_one() {
        let pool = in_memory().await;
        let slots = SqliteSlots::new(pool.clone());
        slots
            .replace_all(&BTreeMap::from([(app(1), 0)]), 1)
            .await
            .unwrap();
        sqlx::query("insert into slots (app_id, slot) values ('app-9', 3)")
            .execute(&pool)
            .await
            .unwrap();
        let wanted = BTreeMap::from([(app(1), 0), (app(2), 3)]);
        assert!(slots.replace_all(&wanted, 4).await.is_err());

        sqlx::query("delete from slots where app_id = 'app-9'")
            .execute(&pool)
            .await
            .unwrap();
        slots.replace_all(&wanted, 4).await.unwrap();
        assert_eq!(slots.all().await.unwrap(), wanted);
        assert_eq!(slots.cursor().await.unwrap(), 4);
    }
}
