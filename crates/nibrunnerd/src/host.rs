//! Everything one host is, in one place: the config it read, what it observed, and the services
//! it acts through. Passed to the reconcile functions rather than reached for globally, so a test
//! builds a host out of recording services and asserts on what they were asked for.

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
    /// The memory a guest may be given, read once at startup rather than per decision: a host is
    /// resized by being replaced, and the number a wake is refused on has to be the number the
    /// report was built from or a full host goes on being placed onto for as long as it refuses.
    pub guest_memory_mib: u64,
    pub state: SharedState,
    /// Shared rather than owned, because a slot is not the network's alone: the tap, the
    /// forward, the addresses and — for the backend that keeps blocks in an object store — the
    /// NBD minor a volume is reached on all come from the same integer. Two copies of that
    /// arithmetic is two answers to which device a tenant's disk is.
    pub allocator: Arc<Mutex<SlotAllocator>>,
    pub cache: Mutex<DesiredStateCache>,
    pub vms: Arc<dyn Vmm>,
    pub volumes: Arc<dyn VolumeBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
    /// This host's own notes. Held open for the life of the daemon rather than opened per write:
    /// the schema is applied on the way up, and a pass that had to wait for a connection would be
    /// a pass whose timing depended on how busy the disk was.
    pub store: sqlx::SqlitePool,
    /// Where a finished bundle goes. Its own store because a bundle is a tenant's whole dataset in
    /// the clear, and it wants different permissions from the artifacts one.
    pub exports: Arc<dyn ExportStore>,
    /// Present only where a volume is something a checkpoint can be cut from. A host on local
    /// files has no server to start, and an export against one says so rather than half-running.
    pub checkpoint_servers: Option<CheckpointServers>,
    pub nbd: NbdDevices,
    pub commands: Arc<dyn CommandRunner>,
    pub firewall: Arc<HostFirewall>,
    pub router: Arc<Router>,
    pub activator: Arc<AppActivator>,
}

impl Host {
    /// The slot an app holds, allocated if it has none. Every per-app resource comes from it, so
    /// a failure here is a host with no room rather than an app with a problem.
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

    /// After the records, and never in place of them: what a request has to arrive to is the slot
    /// and the record together.
    /// Everything this host knows about itself, in one commit.
    ///
    /// All of it or none of it. As five documents a crash between two of them left an app recorded
    /// as running with no slot recorded for it, and the next pass made that worse by allocating a
    /// second slot for the same app — a tenant's port moving because the power went out at the
    /// wrong moment.
    /// The id the control plane assigned, if it ever has. Absent on a host's very first
    /// registration, which is what tells the control plane to assign one.
    pub async fn known_host_id(&self) -> Option<protocol::HostId> {
        let mut connection = self.store.acquire().await.ok()?;
        let held = crate::repositories::host_identity::read(&mut connection)
            .await
            .ok()??;
        protocol::HostId::parse(held).ok()
    }

    /// Written once and never overwritten, so a reinstalled host rejoins as the same host.
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
        // The cursor goes with the slots rather than after them. Written on its own it would point
        // past allocations the next boot has no record of, and the next app would be handed a slot
        // a client is still dialling somebody else on.
        slots::replace_all(&mut tx, &assignments).await?;
        slots::set_cursor(&mut tx, cursor).await?;
        // Only the moment, never the counts it was derived from: the kernel's counters do not
        // outlive the daemon either, because the first apply after a restart rewrites the table.
        activity::replace_all(&mut tx, &snapshot.last_active_at_ms).await?;
        deleted_volumes::replace_all(&mut tx, &deleted).await?;
        tx.commit().await.map_err(crate::repositories::StoreError::write)
    }

    /// What this host was doing when it last wrote anything down.
    ///
    /// A failure here is a host that comes up knowing nothing rather than one that will not come
    /// up: the microVMs are adopted from their pidfiles either way, and everything else is
    /// re-derived by observing. Refusing to start would be the one outcome from which nothing on
    /// this machine recovers.
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

    /// The last document this host was given, so a restart during an outage of whatever writes
    /// the file converges on it rather than on nothing.
    pub async fn cached_desired_state(&self) -> Option<HostDesiredState> {
        crate::desired::read_desired_state(&self.config.cached_desired_state_file())
            .ok()
            .flatten()
    }
}
