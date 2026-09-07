use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{ReportedVolume, VolumeId};
use sqlx::SqlitePool;

use crate::domain::store::StoreError;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait DeletedVolumeRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<VolumeId, ReportedVolume>, StoreError>;
    async fn replace_all(&self, deleted: &BTreeMap<VolumeId, ReportedVolume>) -> Result<(), StoreError>;
}

pub struct SqliteDeletedVolumes {
    pool: SqlitePool,
}

impl SqliteDeletedVolumes {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DeletedVolumeRepository for SqliteDeletedVolumes {
    async fn all(&self) -> Result<BTreeMap<VolumeId, ReportedVolume>, StoreError> {
        let rows = sqlx::query!("select report from deleted_volumes order by volume_id")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let report: ReportedVolume = serde_json::from_str(&row.report).ok()?;
                Some((report.volume_id.clone(), report))
            })
            .collect())
    }

    async fn replace_all(&self, deleted: &BTreeMap<VolumeId, ReportedVolume>) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
        sqlx::query!("delete from deleted_volumes")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::write)?;
        for (volume_id, report) in deleted {
            let volume_id = volume_id.as_str();
            let rendered =
                serde_json::to_string(report).map_err(|error| StoreError::Unwritable(error.to_string()))?;
            sqlx::query!(
                "insert into deleted_volumes (volume_id, report) values (?, ?)",
                volume_id,
                rendered
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
    use crate::test_support::{reported_volume, volume_id};

    fn removed() -> ReportedVolume {
        reported_volume(|report| {
            report.state = protocol::VolumeState::Deleted;
            report.size_bytes = 0;
        })
    }

    async fn repository() -> SqliteDeletedVolumes {
        SqliteDeletedVolumes::new(in_memory().await)
    }

    #[tokio::test]
    async fn what_was_written_is_what_comes_back() {
        let deleted = repository().await;
        let held = BTreeMap::from([(volume_id(), removed())]);
        deleted.replace_all(&held).await.unwrap();
        assert_eq!(deleted.all().await.unwrap(), held);
    }

    #[tokio::test]
    async fn a_host_that_has_removed_nothing_reads_back_nothing() {
        assert!(repository().await.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_removal_the_reader_acknowledged_stops_being_reported() {
        let deleted = repository().await;
        deleted
            .replace_all(&BTreeMap::from([(volume_id(), removed())]))
            .await
            .unwrap();
        deleted.replace_all(&BTreeMap::new()).await.unwrap();
        assert!(deleted.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_report_this_daemon_cannot_read_is_left_out_rather_than_failing_the_load() {
        let pool = in_memory().await;
        sqlx::query("insert into deleted_volumes (volume_id, report) values ('vol-9', '{}')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(SqliteDeletedVolumes::new(pool).all().await.unwrap().is_empty());
    }
}
