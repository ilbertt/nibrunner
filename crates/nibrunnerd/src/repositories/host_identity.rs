//! The name this host reports under.

use sqlx::SqliteConnection;

use crate::repositories::StoreError;

pub async fn read(connection: &mut SqliteConnection) -> Result<Option<String>, StoreError> {
    let held = sqlx::query!("select host_id from host_identity where only_row = 0")
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::read)?;
    Ok(held.map(|row| row.host_id))
}

/// Written once and never overwritten: a host that renamed itself on a restart would look like a
/// second host to whatever is counting them, and the first would look like one that went away.
pub async fn remember(connection: &mut SqliteConnection, host_id: &str) -> Result<(), StoreError> {
    sqlx::query!(
        "insert into host_identity (only_row, host_id) values (0, ?) on conflict do nothing",
        host_id
    )
    .execute(&mut *connection)
    .await
    .map_err(StoreError::write)?;
    Ok(())
}
