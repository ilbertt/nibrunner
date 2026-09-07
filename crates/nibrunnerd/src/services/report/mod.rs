pub mod build_report;
pub mod capacity;
pub mod instance_record;
pub mod routes;
pub mod versions;
pub mod writer;

pub use instance_record::*;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{HostReportedState, HostVersions};

use crate::host::Host;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait ReportService: Send + Sync {
    async fn build(&self) -> HostReportedState;
    async fn publish(&self);
}

pub struct HostReporter {
    host: Arc<Host>,
    versions: HostVersions,
    path: PathBuf,
}

impl HostReporter {
    pub fn new(host: Arc<Host>, versions: HostVersions) -> Arc<Self> {
        let path = writer::reported_state_file(&host);
        Arc::new(Self { host, versions, path })
    }
}

#[async_trait]
impl ReportService for HostReporter {
    async fn build(&self) -> HostReportedState {
        writer::build(&self.host, self.versions.clone()).await
    }

    async fn publish(&self) {
        let report = self.build().await;
        writer::write(&self.path, &report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use protocol::{HostState, InstanceState};

    fn versions() -> HostVersions {
        versions::compiled_versions("v1.16.1", "6.1.180-test")
    }

    fn reporter(host: &TestHost) -> Arc<dyn ReportService> {
        HostReporter::new(host.arc().clone(), versions())
    }

    #[tokio::test]
    async fn a_reporter_says_what_the_host_holds_and_under_which_versions() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        host.state.modify(|snapshot| snapshot.converged = true).await;

        let report = reporter(&host).build().await;
        assert_eq!(report.versions, versions());
        assert_eq!(report.state, HostState::Ready);
        assert_eq!(report.instances.len(), 1);
        assert_eq!(report.instances[0].app_id, app_id());
        assert_eq!(report.instances[0].state, InstanceState::Running);
    }

    #[tokio::test]
    async fn a_host_that_has_not_converged_yet_reports_itself_as_registering() {
        let host = test_host().await;
        assert_eq!(reporter(&host).build().await.state, HostState::Registering);
    }

    #[tokio::test]
    async fn publishing_writes_the_report_to_the_file_the_reporter_named() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        let path = writer::reported_state_file(&host);
        assert!(!path.exists());

        reporter(&host).publish().await;

        let written: HostReportedState = crate::json_store::read_json(&path)
            .unwrap()
            .expect("the report was written where the reporter named");
        assert_eq!(written.instances.len(), 1);
        assert_eq!(written.instances[0].app_id, app_id());
    }

    #[tokio::test]
    async fn each_publish_replaces_the_last_rather_than_leaving_a_stale_one_behind() {
        let host = test_host().await;
        let reporter = reporter(&host);
        let path = writer::reported_state_file(&host);

        host.state.put_record(instance_record(|_| {})).await;
        reporter.publish().await;
        host.state.drop_record(&app_id()).await;
        reporter.publish().await;

        let written: HostReportedState = crate::json_store::read_json(&path).unwrap().unwrap();
        assert!(written.instances.is_empty());
    }

    #[tokio::test]
    async fn a_reporter_that_cannot_write_says_so_rather_than_bringing_the_host_down() {
        let host = test_host().await;
        let path = writer::reported_state_file(&host);
        std::fs::create_dir_all(&path).unwrap();
        reporter(&host).publish().await;
        assert!(path.is_dir());
    }
}
