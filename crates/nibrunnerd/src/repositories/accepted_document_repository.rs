use async_trait::async_trait;
use protocol::Sha256Digest;
use sqlx::SqlitePool;

use crate::desired::AcceptedDocument;
use crate::domain::store::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait AcceptedDocumentRepository: Send + Sync {
    async fn read(&self) -> Result<Option<AcceptedDocument>, StoreError>;
    async fn remember(&self, document: &AcceptedDocument) -> Result<(), StoreError>;
}

pub struct SqliteAcceptedDocument {
    pool: SqlitePool,
}

impl SqliteAcceptedDocument {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AcceptedDocumentRepository for SqliteAcceptedDocument {
    async fn read(&self) -> Result<Option<AcceptedDocument>, StoreError> {
        let held = sqlx::query!("select document, digest from accepted_document where only_row = 0")
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::read)?;
        held.map(|row| {
            Ok(AcceptedDocument {
                desired: serde_json::from_str(&row.document)
                    .map_err(|error| StoreError::Unreadable(error.to_string()))?,
                digest: Sha256Digest::parse(&row.digest)
                    .map_err(|error| StoreError::Unreadable(error.to_string()))?,
            })
        })
        .transpose()
    }

    async fn remember(&self, document: &AcceptedDocument) -> Result<(), StoreError> {
        let rendered = serde_json::to_string(&document.desired)
            .map_err(|error| StoreError::Unwritable(error.to_string()))?;
        let digest = document.digest.as_str();
        sqlx::query!(
            "insert into accepted_document (only_row, document, digest) values (0, ?, ?)
             on conflict (only_row) do update set document = excluded.document, digest = excluded.digest",
            rendered,
            digest
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
    use crate::domain::store::in_memory;
    use crate::test_support::{accepted_document, desired_instance, desired_state};

    async fn repository() -> SqliteAcceptedDocument {
        SqliteAcceptedDocument::new(in_memory().await)
    }

    #[tokio::test]
    async fn a_host_that_was_never_given_a_document_holds_none() {
        assert_eq!(repository().await.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn what_was_remembered_is_what_comes_back() {
        let accepted = repository().await;
        let document = accepted_document(desired_state(|state| {
            state.instances = vec![desired_instance(|_| {})]
        }));
        accepted.remember(&document).await.unwrap();
        assert_eq!(accepted.read().await.unwrap(), Some(document));
    }

    #[tokio::test]
    async fn only_the_last_document_taken_up_is_kept() {
        let accepted = repository().await;
        accepted
            .remember(&accepted_document(desired_state(|_| {})))
            .await
            .unwrap();
        let later = accepted_document(desired_state(|state| {
            state.instances = vec![desired_instance(|_| {})]
        }));
        accepted.remember(&later).await.unwrap();
        assert_eq!(accepted.read().await.unwrap(), Some(later));
    }

    #[tokio::test]
    async fn a_row_that_is_not_a_document_is_a_read_error_rather_than_a_document() {
        let pool = in_memory().await;
        sqlx::query(
            "insert into accepted_document (only_row, document, digest) values (0, '{\"hostId\": 7}', '')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(SqliteAcceptedDocument::new(pool).read().await.is_err());
    }
}
