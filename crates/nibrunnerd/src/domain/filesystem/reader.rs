use protocol::{AppId, FilesystemQuery, FilesystemQueryOutcome, FilesystemQueryResult, GuestPath};

use crate::domain::filesystem::client::{GuestFilesystem, GuestFilesystemError};
use crate::host::Host;

pub fn guest_vsock_path(host: &Host, app_id: &AppId) -> std::path::PathBuf {
    host.config
        .vm_dir()
        .join(app_id.as_str())
        .join(guest_contract::vsock::GUEST_VSOCK_FILENAME)
}

pub async fn served_app_ids(host: &Host) -> Vec<AppId> {
    host.slots().await.into_iter().map(|slot| slot.app_id).collect()
}

pub async fn list(
    host: &Host,
    app_id: &AppId,
    path: &GuestPath,
) -> Result<protocol::DirectoryListing, GuestFilesystemError> {
    let mut guest = GuestFilesystem::dial(app_id, &guest_vsock_path(host, app_id)).await?;
    guest.list(path).await
}

pub async fn answer(host: &Host, query: &FilesystemQuery) -> FilesystemQueryResult {
    let outcome = match list(host, &query.app_id, &query.path).await {
        Ok(listing) => FilesystemQueryOutcome::Listed { listing },
        Err(error) => {
            tracing::warn!(
                query_id = %query.query_id,
                app_id = %query.app_id,
                error = %error.message(),
                "a filesystem read failed"
            );
            FilesystemQueryOutcome::Failed {
                message: error.message(),
            }
        }
    };
    FilesystemQueryResult {
        query_id: query.query_id.clone(),
        outcome,
    }
}

pub async fn measure(host: &Host, app_id: &AppId) -> crate::domain::usage::GuestReading {
    let Ok(mut guest) = GuestFilesystem::dial(app_id, &guest_vsock_path(host, app_id)).await else {
        return crate::domain::usage::GuestReading::default();
    };
    crate::domain::usage::GuestReading {
        filesystem: guest.usage().await.ok(),
        compute: guest.compute().await.ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn a_guest_this_host_cannot_reach_is_answered_rather_than_left_waiting() {
        let host = test_host().await;
        let query = FilesystemQuery {
            query_id: protocol::FilesystemQueryId::parse("q-1").unwrap(),
            app_id: app_id(),
            path: GuestPath::parse("/").unwrap(),
        };
        let result = answer(host.arc(), &query).await;
        assert_eq!(result.query_id, query.query_id);
        let FilesystemQueryOutcome::Failed { message } = result.outcome else {
            panic!("a guest that is not running cannot have listed anything");
        };
        assert!(message.contains("no microVM is running"), "{message}");
    }

    #[tokio::test]
    async fn only_the_apps_this_host_holds_a_slot_for_are_offered() {
        let host = test_host().await;
        assert!(served_app_ids(host.arc()).await.is_empty());
        host.slot_for(&app_id()).await.unwrap();
        assert_eq!(served_app_ids(host.arc()).await, vec![app_id()]);
        host.allocator.lock().await.release(&app_id());
        assert!(served_app_ids(host.arc()).await.is_empty());
    }

    #[tokio::test]
    async fn each_guest_is_reached_on_a_socket_inside_its_own_microvm_directory() {
        let host = test_host().await;
        let neighbour = AppId::parse("app-2").unwrap();
        let path = guest_vsock_path(host.arc(), &app_id());

        assert!(path.starts_with(host.config.vm_dir()));
        assert!(path.ends_with(guest_contract::vsock::GUEST_VSOCK_FILENAME));
        assert_eq!(path.parent().unwrap().file_name().unwrap(), app_id().as_str());
        assert_ne!(path, guest_vsock_path(host.arc(), &neighbour));
    }

    #[tokio::test]
    async fn a_guest_that_is_not_running_measures_as_nothing_rather_than_as_zero() {
        let host = test_host().await;
        assert_eq!(
            measure(host.arc(), &app_id()).await,
            crate::domain::usage::GuestReading::default()
        );
    }

    #[tokio::test]
    async fn a_guest_that_is_not_running_cannot_be_listed() {
        let host = test_host().await;
        let Err(error) = list(host.arc(), &app_id(), &GuestPath::parse("/").unwrap()).await else {
            panic!("a guest that is not running listed something anyway");
        };
        assert!(error.message().contains("no microVM is running"), "{error}");
        assert!(error.message().contains(app_id().as_str()), "{error}");
    }
}
