use std::sync::Arc;

use protocol::HostVersions;

use crate::adapters::control_plane::ControlPlaneClient;
use crate::controllers::control_plane_controller::ControlPlaneController;
use crate::controllers::converge_controller::ConvergeController;
use crate::controllers::filesystem_controller::FilesystemController;
use crate::controllers::measurement_controller::MeasurementController;
use crate::controllers::status_controller::StatusController;
use crate::controllers::Controller;
use crate::domain::control_plane::SessionHolder;
use crate::host::Host;
use crate::services::control_plane_service::RemoteControlPlane;
use crate::services::filesystem_service::GuestFilesystems;
use crate::services::idle_service::HostIdle;
use crate::services::reconcile_service::HostReconciler;
use crate::services::report_service::HostReporter;
use crate::services::report_service::ReportService;
use crate::services::usage_service::HostUsage;

pub struct LifecycleController {
    host: Arc<Host>,
    versions: HostVersions,
}

impl LifecycleController {
    pub fn new(host: Arc<Host>, versions: HostVersions) -> Arc<Self> {
        Arc::new(Self { host, versions })
    }

    pub async fn start(&self) {
        self.host.load().await;
        let adopted = self.host.vms.adopted_app_ids().await;
        if !adopted.is_empty() {
            tracing::info!(adopted = adopted.len(), "microVMs from an earlier daemon adopted");
        }
        crate::domain::reconcile::network::apply_activators(&self.host).await;
        crate::run::serve_proxy(&self.host);
    }

    pub fn controllers(&self) -> Vec<Arc<dyn Controller>> {
        let reconciler = HostReconciler::new(self.host.clone());
        let reports = HostReporter::new(self.host.clone(), self.versions.clone());
        let filesystems = GuestFilesystems::new(self.host.clone());

        let mut held: Vec<Arc<dyn Controller>> = vec![
            ConvergeController::new(self.host.clone(), reconciler.clone()),
            StatusController::new(self.host.clone(), reconciler, reports),
            MeasurementController::new(
                HostIdle::new(self.host.clone()),
                HostUsage::new(self.host.clone(), filesystems.clone()),
            ),
        ];
        if let Some(url) = self.host.config.control_plane_url.clone() {
            tracing::info!(control_plane = %url, "this host will register and poll");
            let sessions = Arc::new(SessionHolder::new(ControlPlaneClient::new(url)));
            let remote = RemoteControlPlane::new(self.host.clone(), sessions);
            held.push(ControlPlaneController::new(remote.clone()));
            held.push(FilesystemController::new(remote, filesystems));
        }
        held
    }

    pub async fn stop(&self) {
        self.host.persist().await;
        HostReporter::new(self.host.clone(), self.versions.clone())
            .publish()
            .await;
    }
}
