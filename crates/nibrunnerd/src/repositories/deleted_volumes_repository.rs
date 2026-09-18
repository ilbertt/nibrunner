use std::collections::BTreeMap;

use async_trait::async_trait;
use protocol::{ReportedVolume, VolumeId};
use sqlx::SqlitePool;

use crate::domain::store::StoreError;
use crate::repositories::last_written::LastWritten;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait DeletedVolumeRepository: Send + Sync {
    async fn all(&self) -> Result<BTreeMap<VolumeId, ReportedVolume>, StoreError>;
    /// Leaves the table holding `deleted` and nothing else, writing only what differs from the
    /// last write.
    async fn replace_all(&self, deleted: &BTreeMap<VolumeId, ReportedVolume>) -> Result<(), StoreError>;
}

pub struct SqliteDeletedVolumes {
    pool: SqlitePool,
    last_written: LastWritten<BTreeMap<String, String>>,
}

impl SqliteDeletedVolumes {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            last_written: LastWritten::unknown(),
        }
    }

    async fn stored(&self) -> Result<BTreeMap<String, String>, StoreError> {
        let rows = sqlx::query!("select volume_id, report from deleted_volumes")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::read)?;
        Ok(rows.into_iter().map(|row| (row.volume_id, row.report)).collect())
    }
}

#[async_trait]
impl DeletedVolumeRepository for SqliteDeletedVolumes {
    async fn all(&self) -> Result<BTreeMap<VolumeId, ReportedVolume>, StoreError> {
        Ok(self
            .stored()
            .await?
            .into_values()
            .filter_map(|report| {
                let report: ReportedVolume = serde_json::from_str(&report).ok()?;
                Some((report.volume_id.clone(), report))
            })
            .collect())
    }

    async fn replace_all(&self, deleted: &BTreeMap<VolumeId, ReportedVolume>) -> Result<(), StoreError> {
        let wanted = deleted
            .iter()
            .map(|(volume_id, report)| {
                let rendered = serde_json::to_string(report)
                    .map_err(|error| StoreError::Unwritable(error.to_string()))?;
                Ok((volume_id.as_str().to_owned(), rendered))
            })
            .collect::<Result<_, StoreError>>()?;
        let delta = self.last_written.towards(wanted, self.stored()).await?;
        if !delta.is_empty() {
            let mut tx = self.pool.begin().await.map_err(StoreError::write)?;
            for (volume_id, report) in delta.changed() {
                sqlx::query!(
                    "insert into deleted_volumes (volume_id, report) values (?, ?)
                     on conflict (volume_id) do update set report = excluded.report",
                    volume_id,
                    report
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::write)?;
            }
            for volume_id in delta.gone() {
                sqlx::query!("delete from deleted_volumes where volume_id = ?", volume_id)
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
    use crate::test_support::{reported_volume, volume_id};

    fn removed() -> ReportedVolume {
        reported_volume(|report| {
            report.state = protocol::VolumeState::Deleted;
            report.size_bytes = 0;
        })
    }

    fn removed_volume(index: u32) -> (VolumeId, ReportedVolume) {
        let report = reported_volume(|report| {
            report.volume_id = VolumeId::parse(format!("vol-{index}")).unwrap();
            report.state = protocol::VolumeState::Deleted;
            report.size_bytes = 0;
        });
        (report.volume_id.clone(), report)
    }

    async fn repository() -> SqliteDeletedVolumes {
        SqliteDeletedVolumes::new(in_memory().await)
    }

    async fn watched() -> (SqlitePool, SqliteDeletedVolumes) {
        let pool = in_memory().await;
        written::watch(&pool, "deleted_volumes", "volume_id").await;
        (pool.clone(), SqliteDeletedVolumes::new(pool))
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

    #[tokio::test]
    async fn a_pass_writes_the_removals_it_changed_and_no_others() {
        let (pool, deleted) = watched().await;
        deleted
            .replace_all(&BTreeMap::from([removed_volume(1), removed_volume(2)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["insert vol-1", "insert vol-2"]);

        deleted
            .replace_all(&BTreeMap::from([removed_volume(2), removed_volume(3)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["insert vol-3", "delete vol-1"]);
    }

    #[tokio::test]
    async fn a_pass_that_changed_nothing_writes_nothing() {
        let (pool, deleted) = watched().await;
        let held = BTreeMap::from([removed_volume(1)]);
        deleted.replace_all(&held).await.unwrap();
        written::writes(&pool).await;

        deleted.replace_all(&held).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn a_restart_diffs_against_what_the_table_holds_rather_than_writing_over_it() {
        let (pool, before) = watched().await;
        let held = BTreeMap::from([removed_volume(1), removed_volume(2)]);
        before.replace_all(&held).await.unwrap();
        written::writes(&pool).await;

        let after = SqliteDeletedVolumes::new(pool.clone());
        after.replace_all(&held).await.unwrap();
        assert!(written::writes(&pool).await.is_empty());

        after
            .replace_all(&BTreeMap::from([removed_volume(2)]))
            .await
            .unwrap();
        assert_eq!(written::writes(&pool).await, ["delete vol-1"]);
    }
}
