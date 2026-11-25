use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{AppId, CheckpointId, DesiredVolume, VolumeId};

use super::profile::{
    failed, io_error, sync_dir, write_new, Placement, CACHE_MEMORY_MIB, DISK_GIB, RETIRED, WRITER_CONFIG,
};
use super::{mounted, CustomerPlacements};
use crate::adapters::volumes::zerofs::{ZerofsFilesystem, ZerofsVolumes};
use crate::adapters::volumes::{
    AttachedVolume, CacheReservation, ObservedBacking, VolumeBackend, VolumeError,
};
use crate::domain::exports::reader::CheckpointServer;
use crate::install::{customer_writer::MOUNT_ROOT, service_user};
use crate::ports::{CommandRequest, CommandRunner, CommandRunnerExt};

pub(crate) struct CustomerVolumes {
    pub(crate) placements: CustomerPlacements,
    pub(super) shared: ZerofsVolumes,
    operations: std::sync::Mutex<BTreeMap<AppId, Arc<tokio::sync::Mutex<()>>>>,
    resolver: std::sync::Mutex<BTreeMap<AppId, Arc<super::dns::EndpointHosts>>>,
    pub(super) checkpoint_owners: PathBuf,
    pub(super) checkpoint_lock: tokio::sync::Mutex<()>,
    caches: PathBuf,
    cache_owner: (u32, u32),
    commands: Arc<dyn CommandRunner>,
}

impl CustomerVolumes {
    pub(crate) fn new(
        placements: CustomerPlacements,
        shared: ZerofsVolumes,
        caches: PathBuf,
        cache_owner: (u32, u32),
        commands: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            checkpoint_owners: placements.root.with_file_name("checkpoint-owners"),
            checkpoint_lock: Default::default(),
            placements,
            operations: std::sync::Mutex::new(BTreeMap::new()),
            resolver: Default::default(),
            shared,
            caches,
            cache_owner,
            commands,
        }
    }

    pub(crate) fn prepare(
        &self,
        desired: &protocol::HostDesiredState,
        guest_budget_mib: u64,
        records: &BTreeMap<AppId, crate::domain::report::InstanceRecord>,
        previous: Option<&protocol::HostDesiredState>,
    ) -> Result<(), VolumeError> {
        use crate::domain::report::capacity::committed_resources;
        let held = self.placements.held.read().expect("placement lock");
        let new: Vec<_> = desired
            .volumes
            .iter()
            .filter(|v| v.storage.is_some() && !held.contains_key(&v.app_id))
            .collect();
        if !new.is_empty() {
            if new.iter().any(|v| {
                records
                    .values()
                    .any(|r| (r.app_id == v.app_id) != (r.volume_id == v.volume_id))
            }) {
                return Err(failed("the guest and source volume have different owners"));
            }
            let instances: Vec<_> = records.values().cloned().collect();
            let committed: u64 = committed_resources(&instances)
                .iter()
                .map(|r| u64::from(r.memory_mib))
                .sum();
            let count = new.len() as u64;
            if guest_budget_mib.saturating_sub(committed) < count * u64::from(CACHE_MEMORY_MIB) {
                return Err(failed("insufficient host memory for customer writer caches"));
            }
            drop(held);
            let snapshot_dir = self
                .caches
                .parent()
                .expect("cache is inside the snapshot directory");
            let disk = crate::adapters::vm::snapshot::measure_disk_under(
                snapshot_dir,
                self.reserved_cache().disk_bytes,
            )
            .map_err(io_error)?;
            if crate::adapters::vm::snapshot::refusal_for_disk(&disk, count * DISK_GIB * 1_073_741_824)
                .is_some()
            {
                return Err(failed("insufficient disk space for customer writer caches"));
            }
        } else {
            drop(held);
        }
        let claims: std::collections::BTreeSet<_> = new.iter().map(|v| &v.volume_id).collect();
        let mut owners = BTreeMap::new();
        if let Some(previous) = previous {
            for v in previous.volumes.iter().filter(|v| claims.contains(&v.volume_id)) {
                if v.storage.is_none() && v.desired_state == protocol::DesiredPresence::Present {
                    owners.insert(v.volume_id.clone(), v.app_id.clone());
                }
            }
        }
        for r in records.values().filter(|r| claims.contains(&r.volume_id)) {
            if owners
                .insert(r.volume_id.clone(), r.app_id.clone())
                .is_some_and(|owner| owner != r.app_id)
            {
                return Err(failed("shared image ownership is ambiguous"));
            }
        }
        self.placements.prepare(desired, &owners, previous)
    }

    fn operation_lock(&self, app: &AppId) -> Arc<tokio::sync::Mutex<()>> {
        self.operations
            .lock()
            .expect("customer writer operation lock")
            .entry(app.clone())
            .or_default()
            .clone()
    }

    pub(super) fn try_operation(&self, app: &AppId) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.operation_lock(app).try_lock_owned().ok()
    }

    async fn operation(&self, app: &AppId) -> tokio::sync::OwnedMutexGuard<()> {
        self.operation_lock(app).lock_owned().await
    }

    fn shared_source(&self, app: &AppId) -> bool {
        self.placements
            .held
            .read()
            .expect("placement lock")
            .get(app)
            .is_some_and(|p| self.serves_shared(p))
    }

    fn placement(&self, app: &AppId, volume: &VolumeId) -> Result<Option<Arc<Placement>>, VolumeError> {
        let held = self.placements.held.read().expect("placement lock");
        match held.get(app) {
            Some(p) if &p.volume == volume => {
                match super::migration::Migration::load(&p.directory(&self.placements.root))? {
                    Some(m) if m.serves_shared() => Ok(None),
                    Some(m) if m.fenced() => Err(failed("volume migration in progress")),
                    _ => Ok(Some(p.clone())),
                }
            }
            Some(_) => Err(failed("this customer app owns a different volume")),
            None if held.values().any(|p| &p.volume == volume) => {
                Err(failed("this customer volume belongs to a different app"))
            }
            None => Ok(None),
        }
    }

    pub(super) fn backend(&self, p: &Placement) -> ZerofsVolumes {
        let directory = p.directory(&self.placements.root);
        let mount = Path::new(MOUNT_ROOT).join(p.app.as_str());
        self.shared.on_filesystem(ZerofsFilesystem {
            config_file: directory.join(WRITER_CONFIG),
            storage_prefix: p.prefix.clone(),
            mount_path: mount,
            nbd_socket_path: p.socket("nbd.sock"),
            binary: self.shared.filesystem().binary.clone(),
            readers: None,
        })
    }

    async fn service(&self, action: &str, name: &str, app: &AppId) -> Result<(), VolumeError> {
        let unit = format!("nibrunner-customer-{name}@{app}.service");
        let mut request = CommandRequest::new(&["systemctl", action, &unit]);
        request.timeout = std::time::Duration::from_secs(150);
        self.commands
            .stdout_of(request)
            .await
            .map_err(|_| failed(format!("could not {action} {unit}")))?;
        Ok(())
    }

    fn resolver(&self, p: &Placement) -> Result<Arc<super::dns::EndpointHosts>, VolumeError> {
        let mut resolvers = self.resolver.lock().expect("customer endpoint resolver lock");
        if let Some(held) = resolvers.get(&p.app) {
            return Ok(held.clone());
        }
        let hosts = Arc::new(super::dns::EndpointHosts::new(
            &p.endpoint,
            &p.directory(&self.placements.root),
        )?);
        resolvers.insert(p.app.clone(), hosts.clone());
        Ok(hosts)
    }

    pub(super) async fn active(&self, p: &Placement) -> Result<ZerofsVolumes, VolumeError> {
        if p.directory(&self.placements.root)
            .join(RETIRED)
            .try_exists()
            .map_err(io_error)?
        {
            return Err(failed("this customer writer has been retired"));
        }
        self.resolver(p)?.prepare().await?;
        let cache = self.caches.join(p.app.as_str());
        crate::json_store::make_directory(&self.caches, 0o700).map_err(io_error)?;
        crate::json_store::make_directory(&cache, 0o700).map_err(io_error)?;
        service_user::own(&cache, self.cache_owner)
            .map_err(|_| failed("could not prepare the customer writer cache"))?;
        self.service("start", "mount", &p.app).await?;
        let backend = self.backend(p);
        if !mounted(&backend.filesystem().mount_path)? {
            return Err(failed("the customer writer has no mounted filesystem"));
        }
        Ok(backend)
    }

    pub(crate) fn reservation(&self) -> CacheReservation {
        let count = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .values()
            .filter(|p| !p.directory(&self.placements.root).join("released").is_file())
            .count() as u64;
        CacheReservation {
            disk_bytes: count * DISK_GIB * 1_073_741_824,
            memory_bytes: count * u64::from(CACHE_MEMORY_MIB) * 1_048_576,
        }
    }

    pub(super) async fn stop_writer(&self, p: &Placement) -> Result<(), VolumeError> {
        self.service("stop", "mount", &p.app).await?;
        self.service("stop", "zerofs", &p.app).await
    }

    pub(super) async fn withdraw_rejected(&self, p: &Placement) -> Result<(), VolumeError> {
        // A rejected attempt has stopped its private writer and released its dedicated attachment.
        if super::images::image_size(&self.shared.filesystem().mount_path, &p.volume)?
            .is_none_or(|bytes| bytes == 0)
        {
            return Err(failed("cannot withdraw a migration whose shared disk is missing"));
        }
        self.stop_writer(p).await?;
        self.clear_cache(p)?;
        let mut held = self.placements.held.write().expect("placement lock");
        let dir = p.directory(&self.placements.root);
        let discarded = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(&self.placements.root)
            .map_err(io_error)?;
        let target = discarded.path().join("withdrawn");
        std::fs::rename(&dir, target).map_err(io_error)?;
        held.remove(&p.app);
        self.resolver
            .lock()
            .expect("customer endpoint resolver lock")
            .remove(&p.app);
        sync_dir(&self.placements.root)?;
        discarded.close().map_err(io_error)
    }

    fn clear_cache(&self, p: &Placement) -> Result<(), VolumeError> {
        match std::fs::remove_dir_all(self.caches.join(p.app.as_str())) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }

    async fn retire(&self, p: &Placement) -> Result<(), VolumeError> {
        let dir = p.directory(&self.placements.root);
        let retired = dir.join(RETIRED);
        let released = dir.join("released");
        if released.try_exists().map_err(io_error)? {
            return Ok(());
        }
        if !retired.try_exists().map_err(io_error)? {
            let backend = self.active(p).await?;
            backend.flush_customer(&p.volume, &p.app).await?;
            backend.detach(&p.volume, &p.app).await?;
            super::images::remove(&backend.filesystem().mount_path, &p.volume)?;
            backend.flush(&p.app).await?;
            write_new(&retired, "")?;
            sync_dir(&dir)?;
        }
        self.service("stop", "mount", &p.app).await?;
        self.service("stop", "zerofs", &p.app).await?;
        if mounted(&Path::new(MOUNT_ROOT).join(p.app.as_str()))? {
            return Err(failed("the retired writer is still mounted"));
        }
        self.clear_cache(p)?;
        remove_file(&dir.join(WRITER_CONFIG))?;
        write_new(&released, "")?;
        sync_dir(&dir)
    }
}

pub(super) fn remove_file(path: &Path) -> Result<(), VolumeError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}

#[async_trait]
impl VolumeBackend for CustomerVolumes {
    async fn provision(&self, desired: &DesiredVolume) -> Result<AttachedVolume, VolumeError> {
        match (
            self.placement(&desired.app_id, &desired.volume_id)?,
            desired.storage.as_ref(),
        ) {
            (None, None) => self.shared.provision(desired).await,
            (None, Some(_)) if self.shared_source(&desired.app_id) => {
                self.shared
                    .verify_migrated_disk(&desired.volume_id, &desired.app_id)
                    .await?;
                self.shared.provision(desired).await
            }
            (Some(p), Some(_)) => {
                let _operation = self.operation(&p.app).await;
                self.active(&p).await?.provision(desired).await
            }
            _ => Err(failed("volume storage does not match its placement")),
        }
    }

    async fn attach(&self, volume: &VolumeId, app: &AppId) -> Result<AttachedVolume, VolumeError> {
        match self.placement(app, volume)? {
            None => self.shared.attach(volume, app).await,
            Some(p) => {
                let _operation = self.operation(&p.app).await;
                self.active(&p).await?.attach(volume, app).await
            }
        }
    }

    async fn detach(&self, volume: &VolumeId, app: &AppId) -> Result<(), VolumeError> {
        match self.placement(app, volume)? {
            None => self.shared.detach(volume, app).await,
            Some(p) => {
                let _operation = self.operation(&p.app).await;
                self.backend(&p).detach(volume, app).await
            }
        }
    }

    async fn teardown(&self, volume: &VolumeId, app: &AppId) -> Result<(), VolumeError> {
        let _operation = self.operation(app).await;
        let staged = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .get(app)
            .cloned();
        if let Some(p) = staged.filter(|p| &p.volume == volume) {
            let dir = p.directory(&self.placements.root);
            if super::migration::Migration::load(&dir)?.is_some_and(|m| m.serves_shared()) {
                self.stop_writer(&p).await?;
                self.shared.quiesce_source(volume, app).await?;
                super::images::remove(&self.shared.filesystem().mount_path, volume)?;
                self.shared.flush(app).await?;
                if !dir.join(RETIRED).try_exists().map_err(io_error)? {
                    write_new(&dir.join(RETIRED), "shared")?;
                    sync_dir(&dir)?;
                }
                self.clear_cache(&p)?;
                remove_file(&dir.join(WRITER_CONFIG))?;
                if !dir.join("released").try_exists().map_err(io_error)? {
                    write_new(&dir.join("released"), "")?;
                }
                remove_file(&dir.join(super::migration::JOURNAL))?;
                return sync_dir(&dir);
            }
        }
        match self.placement(app, volume)? {
            None => self.shared.teardown(volume, app).await,
            Some(p) => {
                if super::migration::Migration::load(&p.directory(&self.placements.root))?
                    .is_some_and(|m| m.removing_shared_source())
                {
                    return Err(failed(
                        "shared source cleanup must finish before deleting the committed customer disk",
                    ));
                }
                self.retire(&p).await
            }
        }
    }

    async fn flush(&self, app: &AppId) -> Result<(), VolumeError> {
        let p = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .get(app)
            .cloned();
        match p {
            None => self.shared.flush(app).await,
            Some(p)
                if super::migration::Migration::load(&p.directory(&self.placements.root))?
                    .is_some_and(|m| m.serves_shared() || m.fenced()) =>
            {
                self.shared.flush(app).await
            }
            Some(p) => {
                let _operation = self.operation(&p.app).await;
                self.backend(&p).flush_customer(&p.volume, app).await
            }
        }
    }

    async fn observe(&self, owners: &BTreeMap<VolumeId, AppId>) -> Vec<ObservedBacking> {
        let customers: Vec<_> = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .values()
            .filter(|p| !self.serves_shared(p))
            .cloned()
            .collect();
        let mut shared_owners = owners.clone();
        for p in &customers {
            shared_owners.remove(&p.volume);
        }
        let mut observed = self.shared.observe(&shared_owners).await;
        for p in customers {
            observed.retain(|o| o.volume_id != p.volume);
            if p.directory(&self.placements.root).join("released").is_file() {
                continue;
            }
            if let Ok(resolver) = self.resolver(&p) {
                resolver.refresh();
            }
            let backend = self.backend(&p);
            let disk = if mounted(&backend.filesystem().mount_path).unwrap_or(false) {
                backend
                    .observe(&BTreeMap::from([(p.volume.clone(), p.app.clone())]))
                    .await
                    .into_iter()
                    .find(|disk| disk.volume_id == p.volume)
            } else {
                None
            };
            // The placement still needs teardown if its image was removed just before a crash.
            observed.push(disk.unwrap_or_else(|| ObservedBacking {
                volume_id: p.volume.clone(),
                size_bytes: 0,
                attached: false,
                formatted: false,
                device_path: None,
                storage_prefix: p.prefix.clone(),
            }));
        }
        observed
    }

    fn reserved_cache(&self) -> CacheReservation {
        let shared = self.shared.reserved_cache();
        let private = self.reservation();
        CacheReservation {
            disk_bytes: shared.disk_bytes.saturating_add(private.disk_bytes),
            memory_bytes: shared.memory_bytes.saturating_add(private.memory_bytes),
        }
    }

    async fn create_checkpoint(
        &self,
        checkpoint: &CheckpointId,
        volume: &VolumeId,
    ) -> Result<(), VolumeError> {
        let _guard = self.checkpoint_lock.lock().await;
        self.shared.create_checkpoint(checkpoint, volume).await?;
        let generation = self
            .shared
            .checkpoint_inventory()
            .await?
            .remove(checkpoint.as_str())
            .ok_or_else(|| failed("created checkpoint missing from shared writer"))?;
        super::checkpoints::record(&self.checkpoint_owners, checkpoint, volume, generation)
    }
    async fn delete_checkpoint(&self, checkpoint: &CheckpointId) -> Result<(), VolumeError> {
        let _guard = self.checkpoint_lock.lock().await;
        self.shared.delete_checkpoint(checkpoint).await?;
        remove_file(&self.checkpoint_owners.join(checkpoint.as_str()))
    }
    async fn observe_checkpoints(&self) -> Vec<CheckpointId> {
        self.shared.observe_checkpoints().await
    }
    async fn open_checkpoint(
        &self,
        volume: &VolumeId,
        checkpoint: &CheckpointId,
    ) -> Result<CheckpointServer, VolumeError> {
        if self
            .placements
            .held
            .read()
            .expect("placement lock")
            .values()
            .any(|p| &p.volume == volume)
        {
            return Err(failed("this customer volume has no checkpoint reader"));
        }
        self.shared.open_checkpoint(volume, checkpoint).await
    }
}
