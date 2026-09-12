use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{HostReportedState, HostVersions};

use crate::domain::report::writer;
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
        if writer::write(&self.path, &report) {
            self.host
                .metrics
                .passes
                .report_written(report.reported_at.epoch_ms());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    fn versions() -> HostVersions {
        crate::domain::report::versions::compiled_versions("v1.16.1", "6.1.180-test")
    }
    use protocol::{HostState, InstanceState};

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
        assert!(
            page(&host, &written).contains(&format!(
                "nibrunner_report_written_timestamp_seconds {}\n",
                crate::domain::metrics::as_seconds(written.reported_at.epoch_ms() as u64)
            )),
            "the scrape says when the file was last written"
        );
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
        let report = reporter(&host).build().await;
        assert!(page(&host, &report).contains("nibrunner_report_written_timestamp_seconds 0.000\n"));
    }

    fn page(host: &TestHost, report: &HostReportedState) -> String {
        crate::domain::metrics::render(report, &host.metrics, &crate::state::HostSnapshot::default(), 0)
    }

    fn reporter(host: &TestHost) -> Arc<dyn ReportService> {
        HostReporter::new(host.arc().clone(), versions())
    }
}
