use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::repositories::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait HostIdentityRepository: Send + Sync {
    async fn read(&self) -> Result<Option<String>, StoreError>;
    async fn remember(&self, host_id: &str) -> Result<(), StoreError>;
}

pub struct SqliteHostIdentity {
    pool: SqlitePool,
}

impl SqliteHostIdentity {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl HostIdentityRepository for SqliteHostIdentity {
    async fn read(&self) -> Result<Option<String>, StoreError> {
        let held = sqlx::query!("select host_id from host_identity where only_row = 0")
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(held.map(|row| row.host_id))
    }

    async fn remember(&self, host_id: &str) -> Result<(), StoreError> {
        sqlx::query!(
            "insert into host_identity (only_row, host_id) values (0, ?) on conflict do nothing",
            host_id
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::write)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::in_memory;

    async fn repository() -> SqliteHostIdentity {
        SqliteHostIdentity::new(in_memory().await)
    }

    #[tokio::test]
    async fn a_host_that_has_never_registered_has_no_id_to_send() {
        assert_eq!(repository().await.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn what_was_remembered_is_what_comes_back() {
        let identity = repository().await;
        identity.remember("host-1").await.unwrap();
        assert_eq!(identity.read().await.unwrap().as_deref(), Some("host-1"));
    }

    #[tokio::test]
    async fn a_host_that_registered_once_never_renames_itself() {
        let identity = repository().await;
        identity.remember("host-1").await.unwrap();
        identity.remember("host-2").await.unwrap();
        assert_eq!(identity.read().await.unwrap().as_deref(), Some("host-1"));
    }
}
