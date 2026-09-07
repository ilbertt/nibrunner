use std::sync::Arc;

use protocol::{AppId, HostDesiredState};
use tokio::sync::Mutex;

use crate::adapters::net::allocator::SlotAllocator;
use crate::adapters::net::firewall::HostFirewall;
use crate::adapters::proxy::activator::AppActivator;
use crate::adapters::proxy::Router;
use crate::adapters::volumes::nbd::NbdDevices;
use crate::adapters::volumes::VolumeBackend;
use crate::config::HostConfig;
use crate::desired::DesiredStateCache;
use crate::ports::{ArtifactStore, CommandRunner, Vmm};
use crate::services::exports::reader::CheckpointServers;
use crate::services::exports::store::ExportStore;
use crate::state::SharedState;

pub struct Host {
    pub config: HostConfig,
    pub guest_memory_mib: u64,
    pub state: SharedState,
    pub allocator: Arc<Mutex<SlotAllocator>>,
    pub cache: Mutex<DesiredStateCache>,
    pub vms: Arc<dyn Vmm>,
    pub volumes: Arc<dyn VolumeBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub store: sqlx::SqlitePool,
    pub exports: Arc<dyn ExportStore>,
    pub checkpoint_servers: Option<CheckpointServers>,
    pub nbd: NbdDevices,
    pub commands: Arc<dyn CommandRunner>,
    pub firewall: Arc<HostFirewall>,
    pub router: Arc<Router>,
    pub activator: Arc<AppActivator>,
}

impl Host {
    pub async fn slot_for(
        &self,
        app_id: &AppId,
    ) -> Result<nft_render::AppSlot, crate::adapters::net::allocator::SlotExhausted> {
        self.allocator.lock().await.allocate(app_id)
    }

    pub async fn slot_of(&self, app_id: &AppId) -> Option<nft_render::AppSlot> {
        self.allocator.lock().await.lookup(app_id)
    }

    pub async fn slots(&self) -> Vec<nft_render::AppSlot> {
        self.allocator.lock().await.slots()
    }

    pub async fn known_host_id(&self) -> Option<protocol::HostId> {
        let mut connection = self.store.acquire().await.ok()?;
        let held = crate::repositories::host_identity::read(&mut connection)
            .await
            .ok()??;
        protocol::HostId::parse(held).ok()
    }

    pub async fn remember_host_id(&self, host_id: &str) {
        let Ok(mut connection) = self.store.acquire().await else {
            return;
        };
        if let Err(error) = crate::repositories::host_identity::remember(&mut connection, host_id).await {
            tracing::warn!(error = %error.message(), "this host could not write down the id it registered under");
        }
    }

    pub async fn persist(&self) {
        if let Err(error) = self.write_down().await {
            tracing::warn!(error = %error.message(), "this host could not write down what it is running");
        }
    }

    async fn write_down(&self) -> Result<(), crate::repositories::StoreError> {
        use crate::repositories::{activity, deleted_volumes, instances, slots};

        let snapshot = self.state.snapshot().await;
        let records: Vec<_> = snapshot.records.values().cloned().collect();
        let deleted = snapshot.deleted_volumes.clone();
        let (assignments, cursor) = {
            let allocator = self.allocator.lock().await;
            (allocator.assignments().clone(), allocator.cursor())
        };

        let mut tx = self
            .store
            .begin()
            .await
            .map_err(crate::repositories::StoreError::write)?;
        instances::replace_all(&mut tx, &records).await?;
        slots::replace_all(&mut tx, &assignments).await?;
        slots::set_cursor(&mut tx, cursor).await?;
        activity::replace_all(&mut tx, &snapshot.last_active_at_ms).await?;
        deleted_volumes::replace_all(&mut tx, &deleted).await?;
        tx.commit().await.map_err(crate::repositories::StoreError::write)
    }

    pub async fn load(&self) {
        use crate::repositories::{activity, deleted_volumes, instances, slots};

        let mut connection = match self.store.acquire().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "this host could not read its own notes");
                return;
            }
        };
        let records = instances::all(&mut connection).await.unwrap_or_default();
        let last_active = activity::all(&mut connection).await.unwrap_or_default();
        let deleted = deleted_volumes::all(&mut connection).await.unwrap_or_default();
        let assignments = slots::all(&mut connection).await.unwrap_or_default();
        let cursor = slots::cursor(&mut connection).await.unwrap_or_default();
        drop(connection);

        let held = records.len();
        self.allocator.lock().await.restore(assignments, cursor);
        self.state
            .modify(|snapshot| {
                snapshot.records = records
                    .into_iter()
                    .map(|record| (record.app_id.clone(), record))
                    .collect();
                snapshot.last_active_at_ms = last_active;
                snapshot.deleted_volumes = deleted;
            })
            .await;
        tracing::info!(
            instances = held,
            slots = self.slots().await.len(),
            "host state loaded"
        );
    }

    pub async fn cached_desired_state(&self) -> Option<HostDesiredState> {
        crate::desired::read_desired_state(&self.config.cached_desired_state_file())
            .ok()
            .flatten()
    }
}
