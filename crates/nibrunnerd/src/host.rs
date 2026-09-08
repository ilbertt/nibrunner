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
use crate::domain::exports::reader::CheckpointServers;
use crate::domain::exports::store::ExportStore;
use crate::ports::{ArtifactStore, CommandRunner, Vmm};
use crate::state::SharedState;

pub struct Host {
    pub config: HostConfig,
    pub guest_memory_mib: u64,
    pub guest_image_version: String,
    pub state: SharedState,
    pub allocator: Arc<Mutex<SlotAllocator>>,
    pub cache: Mutex<DesiredStateCache>,
    pub vms: Arc<dyn Vmm>,
    pub volumes: Arc<dyn VolumeBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub repositories: crate::repositories::Repositories,
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
        let held = self.repositories.identity.read().await.ok()??;
        protocol::HostId::parse(held).ok()
    }

    pub async fn remember_host_id(&self, host_id: &str) {
        if let Err(error) = self.repositories.identity.remember(host_id).await {
            tracing::warn!(error = %error.message(), "this host could not write down the id it registered under");
        }
    }

    pub async fn persist(&self) {
        if let Err(error) = self.write_down().await {
            tracing::warn!(error = %error.message(), "this host could not write down what it is running");
        }
    }

    async fn write_down(&self) -> Result<(), crate::domain::store::StoreError> {
        let snapshot = self.state.snapshot().await;
        let records: Vec<_> = snapshot.records.values().cloned().collect();
        let (assignments, cursor) = {
            let allocator = self.allocator.lock().await;
            (allocator.assignments().clone(), allocator.cursor())
        };

        self.repositories.slots.replace_all(&assignments, cursor).await?;
        self.repositories.instances.replace_all(&records).await?;
        self.repositories
            .activity
            .replace_all(&snapshot.last_active_at_ms)
            .await?;
        self.repositories
            .deleted_volumes
            .replace_all(&snapshot.deleted_volumes)
            .await
    }

    pub async fn load(&self) {
        let records = self.repositories.instances.all().await.unwrap_or_default();
        let last_active = self.repositories.activity.all().await.unwrap_or_default();
        let deleted = self.repositories.deleted_volumes.all().await.unwrap_or_default();
        let assignments = self.repositories.slots.all().await.unwrap_or_default();
        let cursor = self.repositories.slots.cursor().await.unwrap_or_default();

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use crate::domain::store::StoreError;
    use crate::repositories::activity_repository::MockActivityRepository;
    use crate::repositories::deleted_volumes_repository::MockDeletedVolumeRepository;
    use crate::repositories::host_identity_repository::MockHostIdentityRepository;
    use crate::repositories::instances_repository::MockInstanceRepository;
    use crate::repositories::slots_repository::MockSlotRepository;
    use crate::repositories::Repositories;
    use crate::test_support::*;

    fn mocked() -> (
        MockInstanceRepository,
        MockSlotRepository,
        MockActivityRepository,
        MockDeletedVolumeRepository,
        MockHostIdentityRepository,
    ) {
        (
            MockInstanceRepository::new(),
            MockSlotRepository::new(),
            MockActivityRepository::new(),
            MockDeletedVolumeRepository::new(),
            MockHostIdentityRepository::new(),
        )
    }

    fn bundle(
        instances: MockInstanceRepository,
        slots: MockSlotRepository,
        activity: MockActivityRepository,
        deleted_volumes: MockDeletedVolumeRepository,
        identity: MockHostIdentityRepository,
    ) -> Repositories {
        Repositories {
            instances: Arc::new(instances),
            slots: Arc::new(slots),
            activity: Arc::new(activity),
            deleted_volumes: Arc::new(deleted_volumes),
            identity: Arc::new(identity),
        }
    }

    #[tokio::test]
    async fn a_pass_writes_the_slots_before_the_records_that_depend_on_them() {
        let order = Arc::new(AtomicUsize::new(0));
        let (mut instances, mut slots, mut activity, mut deleted, identity) = mocked();

        let at = order.clone();
        let slots_written = Arc::new(AtomicUsize::new(usize::MAX));
        let recorded = slots_written.clone();
        slots.expect_replace_all().returning(move |_, _| {
            recorded.store(at.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
            Ok(())
        });
        let at = order.clone();
        let instances_written = Arc::new(AtomicUsize::new(usize::MAX));
        let recorded = instances_written.clone();
        instances.expect_replace_all().returning(move |_| {
            recorded.store(at.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
            Ok(())
        });
        activity.expect_replace_all().returning(|_| Ok(()));
        deleted.expect_replace_all().returning(|_| Ok(()));

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        host.persist().await;

        assert!(
            slots_written.load(Ordering::SeqCst) < instances_written.load(Ordering::SeqCst),
            "a record written before its slot is one the next pass allocates a second slot for"
        );
    }

    #[tokio::test]
    async fn a_pass_hands_every_repository_what_the_host_is_holding() {
        let (mut instances, mut slots, mut activity, mut deleted, identity) = mocked();
        instances
            .expect_replace_all()
            .times(1)
            .withf(|records| records.len() == 1 && records[0].app_id == app_id())
            .returning(|_| Ok(()));
        slots
            .expect_replace_all()
            .times(1)
            .withf(|assignments, cursor| assignments.get(&app_id()) == Some(&0) && *cursor == 1)
            .returning(|_, _| Ok(()));
        activity
            .expect_replace_all()
            .times(1)
            .withf(|held| held.get(&app_id()) == Some(&77))
            .returning(|_| Ok(()));
        deleted
            .expect_replace_all()
            .times(1)
            .withf(|held| held.contains_key(&volume_id()))
            .returning(|_| Ok(()));

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        host.slot_for(&app_id()).await.unwrap();
        host.state
            .modify(|snapshot| {
                snapshot.records = BTreeMap::from([(app_id(), instance_record(|_| {}))]);
                snapshot.last_active_at_ms = BTreeMap::from([(app_id(), 77)]);
                snapshot.deleted_volumes = BTreeMap::from([(volume_id(), reported_volume(|_| {}))]);
            })
            .await;
        host.persist().await;
    }

    #[tokio::test]
    async fn a_write_that_failed_leaves_the_host_running_rather_than_taking_it_down() {
        let (mut instances, mut slots, activity, deleted, identity) = mocked();
        slots
            .expect_replace_all()
            .returning(|_, _| Err(StoreError::Unwritable("the disk is full".into())));
        instances.expect_replace_all().never();

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        host.persist().await;
    }

    #[tokio::test]
    async fn what_was_written_down_is_what_the_host_comes_back_holding() {
        let (mut instances, mut slots, mut activity, mut deleted, identity) = mocked();
        instances
            .expect_all()
            .returning(|| Ok(vec![instance_record(|_| {})]));
        slots
            .expect_all()
            .returning(|| Ok(BTreeMap::from([(app_id(), 4)])));
        slots.expect_cursor().returning(|| Ok(5));
        activity
            .expect_all()
            .returning(|| Ok(BTreeMap::from([(app_id(), 77)])));
        deleted
            .expect_all()
            .returning(|| Ok(BTreeMap::from([(volume_id(), reported_volume(|_| {}))])));

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        host.load().await;

        assert!(host.state.record(&app_id()).await.is_some());
        assert_eq!(host.slot_of(&app_id()).await.map(|slot| slot.slot), Some(4));
        let snapshot = host.state.snapshot().await;
        assert_eq!(snapshot.last_active_at_ms.get(&app_id()), Some(&77));
        assert!(snapshot.deleted_volumes.contains_key(&volume_id()));
    }

    #[tokio::test]
    async fn a_host_whose_notes_will_not_be_read_comes_up_knowing_nothing_rather_than_not_at_all() {
        let (mut instances, mut slots, mut activity, mut deleted, identity) = mocked();
        let unreadable = || StoreError::Unreadable("the file is not a database".into());
        instances.expect_all().returning(move || Err(unreadable()));
        slots.expect_all().returning(move || Err(unreadable()));
        slots.expect_cursor().returning(move || Err(unreadable()));
        activity.expect_all().returning(move || Err(unreadable()));
        deleted.expect_all().returning(move || Err(unreadable()));

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        host.load().await;
        assert!(host.state.records().await.is_empty());
    }

    #[tokio::test]
    async fn the_id_the_control_plane_assigned_is_written_once_and_read_back() {
        let (instances, slots, activity, deleted, mut identity) = mocked();
        identity
            .expect_remember()
            .times(1)
            .withf(|host_id| host_id == "host-7")
            .returning(|_| Ok(()));
        identity
            .expect_read()
            .returning(|| Ok(Some("host-7".to_string())));

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        host.remember_host_id("host-7").await;
        assert_eq!(
            host.known_host_id().await.map(|held| held.as_str().to_string()),
            Some("host-7".to_string())
        );
    }

    #[tokio::test]
    async fn a_host_id_the_notes_cannot_hold_is_absent_rather_than_wrong() {
        let (instances, slots, activity, deleted, mut identity) = mocked();
        identity
            .expect_read()
            .returning(|| Ok(Some("not a host id".to_string())));

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        assert_eq!(host.known_host_id().await, None);
    }

    #[tokio::test]
    async fn an_id_that_could_not_be_written_is_logged_rather_than_raised() {
        let (instances, slots, activity, deleted, mut identity) = mocked();
        identity
            .expect_remember()
            .returning(|_| Err(StoreError::Unwritable("read-only".into())));

        let host = test_host_with(bundle(instances, slots, activity, deleted, identity)).await;
        host.remember_host_id("host-7").await;
    }
}
