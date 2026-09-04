//! Everything one host is, in one place: the config it read, what it observed, and the services
//! it acts through. Passed to the reconcile functions rather than reached for globally, so a test
//! builds a host out of recording services and asserts on what they were asked for.

use std::sync::Arc;

use protocol::{AppId, HostDesiredState};
use tokio::sync::Mutex;

use crate::config::HostConfig;
use crate::desired::DesiredStateCache;
use crate::net::allocator::SlotAllocator;
use crate::net::firewall::HostFirewall;
use crate::proxy::activator::AppActivator;
use crate::proxy::Router;
use crate::services::{ArtifactStore, Vmm};
use crate::state::SharedState;
use crate::volumes::VolumeBackend;

pub struct Host {
    pub config: HostConfig,
    pub state: SharedState,
    pub allocator: Mutex<SlotAllocator>,
    pub cache: Mutex<DesiredStateCache>,
    pub vms: Arc<dyn Vmm>,
    pub volumes: Arc<dyn VolumeBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub firewall: Arc<HostFirewall>,
    pub router: Arc<Router>,
    pub activator: Arc<AppActivator>,
}

impl Host {
    /// The slot an app holds, allocated if it has none. Every per-app resource comes from it, so
    /// a failure here is a host with no room rather than an app with a problem.
    pub async fn slot_for(&self, app_id: &AppId) -> Result<nft_render::AppSlot, crate::net::allocator::SlotExhausted> {
        self.allocator.lock().await.allocate(app_id)
    }

    pub async fn slot_of(&self, app_id: &AppId) -> Option<nft_render::AppSlot> {
        self.allocator.lock().await.lookup(app_id)
    }

    pub async fn slots(&self) -> Vec<nft_render::AppSlot> {
        self.allocator.lock().await.slots()
    }

    /// After the records, and never in place of them: what a request has to arrive to is the slot
    /// and the record together.
    pub async fn persist(&self) {
        let snapshot = self.state.snapshot().await;
        let records: Vec<_> = snapshot.records.values().cloned().collect();
        if let Err(error) = crate::json_store::write_json(&self.config.instances_file(), &records) {
            tracing::warn!(error = %error.message(), "this host could not write down what it is running");
        }
        if let Err(error) =
            self.allocator.lock().await.persist(&self.config.slots_file(), &self.config.slot_cursor_file())
        {
            tracing::warn!(error = %error.message(), "this host could not write down its slots");
        }
        // Only the moment, never the counts it was derived from: the kernel's counters do not
        // outlive the daemon either, because the first apply after a restart rewrites the table.
        let activity: Vec<_> = snapshot
            .last_active_at_ms
            .iter()
            .map(|(app_id, at_ms)| serde_json::json!({ "appId": app_id, "atMs": at_ms }))
            .collect();
        let _ = crate::json_store::write_json(&self.config.activity_file(), &activity);
        let deleted: Vec<_> = snapshot.deleted_volumes.values().cloned().collect();
        let _ = crate::json_store::write_json(&self.config.deleted_volumes_file(), &deleted);
    }

    /// What survived the last daemon. The counts are not kept with it: the first firewall apply
    /// after a restart rewrites the table, so every counter is zero by the time this is read and
    /// a baseline carried across would be one the kernel has already contradicted.
    pub async fn load(&self) {
        let records = crate::report::instance_record::read_instance_records(
            crate::json_store::read_json(&self.config.instances_file()).ok().flatten(),
        );
        let activity: Vec<serde_json::Value> =
            crate::json_store::read_json(&self.config.activity_file()).ok().flatten().unwrap_or_default();
        let deleted: Vec<protocol::ReportedVolume> =
            crate::json_store::read_json(&self.config.deleted_volumes_file()).ok().flatten().unwrap_or_default();
        let held = records.len();
        self.state
            .modify(|snapshot| {
                snapshot.records = records.into_iter().map(|record| (record.app_id.clone(), record)).collect();
                snapshot.last_active_at_ms = activity
                    .iter()
                    .filter_map(|entry| {
                        let app_id = AppId::parse(entry.get("appId")?.as_str()?).ok()?;
                        Some((app_id, entry.get("atMs")?.as_i64()?))
                    })
                    .collect();
                snapshot.deleted_volumes =
                    deleted.into_iter().map(|report| (report.volume_id.clone(), report)).collect();
            })
            .await;
        tracing::info!(instances = held, slots = self.slots().await.len(), "host state loaded");
    }

    /// The last document this host was given, so a restart during an outage of whatever writes
    /// the file converges on it rather than on nothing.
    pub async fn cached_desired_state(&self) -> Option<HostDesiredState> {
        crate::desired::read_desired_state(&self.config.cached_desired_state_file()).ok().flatten()
    }
}
