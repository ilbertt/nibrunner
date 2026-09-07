pub mod client;
pub mod reader;

use std::sync::Arc;

use async_trait::async_trait;
use protocol::{AppId, DirectoryListing, FilesystemQuery, FilesystemQueryResult, GuestPath};

use crate::host::Host;
use crate::services::filesystem::client::GuestFilesystemError;
use crate::services::usage::GuestReading;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait FilesystemService: Send + Sync {
    async fn served_app_ids(&self) -> Vec<AppId>;
    async fn list(&self, app_id: &AppId, path: &GuestPath) -> Result<DirectoryListing, GuestFilesystemError>;
    async fn answer(&self, query: &FilesystemQuery) -> FilesystemQueryResult;
    async fn measure(&self, app_id: &AppId) -> GuestReading;
}

pub struct GuestFilesystems {
    host: Arc<Host>,
}

impl GuestFilesystems {
    pub fn new(host: Arc<Host>) -> Arc<Self> {
        Arc::new(Self { host })
    }
}

#[async_trait]
impl FilesystemService for GuestFilesystems {
    async fn served_app_ids(&self) -> Vec<AppId> {
        reader::served_app_ids(&self.host).await
    }

    async fn list(&self, app_id: &AppId, path: &GuestPath) -> Result<DirectoryListing, GuestFilesystemError> {
        reader::list(&self.host, app_id, path).await
    }

    async fn answer(&self, query: &FilesystemQuery) -> FilesystemQueryResult {
        reader::answer(&self.host, query).await
    }

    async fn measure(&self, app_id: &AppId) -> GuestReading {
        reader::measure(&self.host, app_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn only_the_apps_this_host_holds_a_slot_for_are_offered() {
        let host = test_host().await;
        let filesystems = GuestFilesystems::new(host.arc().clone());
        assert!(filesystems.served_app_ids().await.is_empty());
        host.slot_for(&app_id()).await.unwrap();
        assert_eq!(filesystems.served_app_ids().await, vec![app_id()]);
    }

    #[tokio::test]
    async fn a_guest_that_is_not_running_cannot_be_listed() {
        let host = test_host().await;
        let filesystems = GuestFilesystems::new(host.arc().clone());
        let error = filesystems
            .list(&app_id(), &GuestPath::parse("/").unwrap())
            .await
            .unwrap_err();
        assert!(error.message().contains("no microVM is running"), "{error}");
    }

    #[tokio::test]
    async fn a_guest_that_is_not_running_measures_as_nothing_rather_than_as_zero() {
        let host = test_host().await;
        let filesystems = GuestFilesystems::new(host.arc().clone());
        assert_eq!(filesystems.measure(&app_id()).await, GuestReading::default());
    }

    #[tokio::test]
    async fn a_read_that_failed_is_still_an_answer_carrying_the_id_that_was_asked() {
        let host = test_host().await;
        let filesystems = GuestFilesystems::new(host.arc().clone());
        let query = FilesystemQuery {
            query_id: protocol::FilesystemQueryId::parse("q-1").unwrap(),
            app_id: app_id(),
            path: GuestPath::parse("/").unwrap(),
        };
        let result = filesystems.answer(&query).await;
        assert_eq!(result.query_id, query.query_id);
        assert!(matches!(
            result.outcome,
            protocol::FilesystemQueryOutcome::Failed { .. }
        ));
    }
}
