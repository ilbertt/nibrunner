use std::collections::BTreeMap;

use protocol::AppId;
use sqlx::SqliteConnection;

use crate::repositories::StoreError;

pub async fn all(connection: &mut SqliteConnection) -> Result<BTreeMap<AppId, u32>, StoreError> {
    let rows = sqlx::query!("select app_id, slot from slots order by slot")
        .fetch_all(&mut *connection)
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

pub async fn replace_all(
    connection: &mut SqliteConnection,
    assignments: &BTreeMap<AppId, u32>,
) -> Result<(), StoreError> {
    sqlx::query!("delete from slots")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::write)?;
    for (app_id, slot) in assignments {
        let app_id = app_id.as_str();
        let slot = i64::from(*slot);
        sqlx::query!("insert into slots (app_id, slot) values (?, ?)", app_id, slot)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::write)?;
    }
    Ok(())
}

pub async fn cursor(connection: &mut SqliteConnection) -> Result<i64, StoreError> {
    let held = sqlx::query!("select cursor from slot_cursor where only_row = 0")
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::read)?;
    Ok(held.map_or(0, |row| row.cursor))
}

pub async fn set_cursor(connection: &mut SqliteConnection, cursor: i64) -> Result<(), StoreError> {
    sqlx::query!(
        "insert into slot_cursor (only_row, cursor) values (0, ?)
         on conflict (only_row) do update set cursor = excluded.cursor",
        cursor
    )
    .execute(&mut *connection)
    .await
    .map_err(StoreError::write)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::in_memory;

    fn app(index: u32) -> AppId {
        AppId::parse(format!("app-{index}")).expect("a fixture is a valid app id")
    }

    #[tokio::test]
    async fn what_was_written_is_what_comes_back() {
        let pool = in_memory().await;
        let mut connection = pool.acquire().await.unwrap();

        let held = BTreeMap::from([(app(1), 0), (app(2), 5)]);
        replace_all(&mut connection, &held).await.unwrap();
        set_cursor(&mut connection, 6).await.unwrap();

        assert_eq!(all(&mut connection).await.unwrap(), held);
        assert_eq!(cursor(&mut connection).await.unwrap(), 6);
    }

    #[tokio::test]
    async fn a_slot_that_was_released_does_not_survive_the_next_write() {
        let pool = in_memory().await;
        let mut connection = pool.acquire().await.unwrap();

        replace_all(&mut connection, &BTreeMap::from([(app(1), 0), (app(2), 1)]))
            .await
            .unwrap();
        replace_all(&mut connection, &BTreeMap::from([(app(2), 1)]))
            .await
            .unwrap();

        let held = all(&mut connection).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held.get(&app(1)), None);
    }

    #[tokio::test]
    async fn a_host_with_no_cursor_yet_reads_zero_rather_than_failing() {
        let pool = in_memory().await;
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(cursor(&mut connection).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn two_apps_cannot_hold_the_same_slot() {
        let pool = in_memory().await;
        let mut connection = pool.acquire().await.unwrap();
        replace_all(&mut connection, &BTreeMap::from([(app(1), 0)]))
            .await
            .unwrap();
        let clash = sqlx::query("insert into slots (app_id, slot) values ('app-2', 0)")
            .execute(&mut *connection)
            .await;
        assert!(clash.is_err(), "the second app took a slot the first holds");
    }
}
