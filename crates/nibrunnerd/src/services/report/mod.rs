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
