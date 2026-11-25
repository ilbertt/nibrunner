use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use protocol::{AppId, DesiredInstanceState, HostDesiredState};
use serde::{Deserialize, Serialize};

use super::profile::{failed, io_error, sync_dir, Placement};
use super::runtime::CustomerVolumes;
use crate::adapters::volumes::{VolumeBackend, VolumeError};
use crate::host::Host;

pub(super) const JOURNAL: &str = "migration.json";

#[derive(Deserialize, Serialize)]
pub(super) struct Migration {
    attempt: String,
    verified: bool,
    #[serde(default)]
    needs_guest_freeze: bool,
    #[serde(default)]
    retained: Option<super::checkpoints::Inventory>,
}

impl Migration {
    pub(super) fn new() -> Self {
        Self {
            attempt: uuid::Uuid::new_v4().to_string(),
            verified: false,
            needs_guest_freeze: false,
            retained: None,
        }
    }

    pub(super) fn load(dir: &Path) -> Result<Option<Self>, VolumeError> {
        let mut input = match File::open(dir.join(JOURNAL)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(e)),
        };
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes).map_err(io_error)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| failed("volume migration journal is corrupt"))
    }

    pub(super) fn save(&self, dir: &Path) -> Result<(), VolumeError> {
        let mut staged = tempfile::NamedTempFile::new_in(dir).map_err(io_error)?;
        let json = serde_json::to_vec(self).map_err(|_| failed("cannot encode volume migration"))?;
        staged
            .write_all(&json)
            .and_then(|()| staged.as_file().sync_all())
            .map_err(io_error)?;
        staged.persist(dir.join(JOURNAL)).map_err(|e| io_error(e.error))?;
        sync_dir(dir)
    }
}

impl CustomerVolumes {
    pub(crate) fn cleanup_pending(&self, app: &AppId) -> bool {
        self.placements
            .held
            .read()
            .expect("placement lock")
            .get(app)
            .is_some_and(|p| {
                Migration::load(&p.directory(&self.placements.root))
                    .ok()
                    .flatten()
                    .is_some_and(|m| m.retained.is_some())
            })
    }

    pub(crate) fn migrating(&self, app: &AppId) -> bool {
        self.placements
            .held
            .read()
            .expect("placement lock")
            .get(app)
            .is_some_and(|p| match Migration::load(&p.directory(&self.placements.root)) {
                Ok(Some(migration)) => migration.retained.is_none(),
                Ok(None) => false,
                Err(_) => true,
            })
    }

    pub(crate) fn during_migrations<'a>(
        &self,
        desired: &'a HostDesiredState,
    ) -> std::borrow::Cow<'a, HostDesiredState> {
        let mut effective = std::borrow::Cow::Borrowed(desired);
        for (i, instance) in desired.instances.iter().enumerate() {
            if self.migrating(&instance.app_id) {
                effective.to_mut().instances[i].desired_state = DesiredInstanceState::Stopped;
            }
        }
        // A checkpoint on the former shared writer cannot be recreated on the new disk.
        // Explicit releases of older untracked checkpoints wait for the verified cutover.
        let suppress = |checkpoint: &protocol::DesiredCheckpoint| {
            desired
                .volumes
                .iter()
                .find(|v| v.volume_id == checkpoint.volume_id && v.storage.is_some())
                .is_some_and(|v| {
                    checkpoint.desired_state == protocol::DesiredPresence::Present
                        || self.migrating(&v.app_id)
                })
        };
        if desired.checkpoints.iter().any(suppress) {
            effective.to_mut().checkpoints.retain(|c| !suppress(c));
        }
        effective
    }

    pub(crate) fn resume_migrations(self: &Arc<Self>, host: &Arc<Host>, desired: &HostDesiredState) {
        let placements: Vec<_> = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .values()
            .cloned()
            .collect();
        for p in placements {
            match Migration::load(&p.directory(&self.placements.root)) {
                Ok(Some(migration))
                    if migration.verified
                        || desired
                            .volumes
                            .iter()
                            .any(|v| v.app_id == p.app && v.storage.is_some()) => {}
                Err(error) => {
                    tracing::error!(app_id = %p.app, %error, "cannot resume volume migration");
                    continue;
                }
                _ => continue,
            }
            let Some(operation) = self.try_operation(&p.app) else {
                continue;
            };
            let (host, volumes) = (host.clone(), self.clone());
            tokio::spawn(async move {
                let _operation = operation;
                if let Err(error) = volumes.migrate(&host, &p).await {
                    tracing::warn!(app_id = %p.app, %error, "volume migration will retry");
                    host.state
                        .modify(|state| {
                            if let Some(report) =
                                state.volume_reports.iter_mut().find(|v| v.volume_id == p.volume)
                            {
                                report.message = Some(protocol::StateMessage::new(format!(
                                    "volume migration will retry: {error}"
                                )));
                            }
                        })
                        .await;
                    host.state.signal_report();
                }
            });
        }
    }

    async fn stop_source(
        &self,
        host: &Host,
        p: &Placement,
        dir: &Path,
        migration: &mut Migration,
    ) -> Result<(), VolumeError> {
        let _transition = host.state.transition(&p.app).await;
        host.cron_runs.stop(&p.app).await;
        host.cron_execution
            .stop(&p.app)
            .await
            .map_err(|_| failed("could not drain guest cron runs"))?;
        host.close_http_connections_to(&p.app).await;
        host.activator.close_connections_to(&p.app).await;
        let active = |states: &std::collections::BTreeMap<_, crate::adapters::vm::VmStatus>| {
            states
                .get(&p.app)
                .map(|s| s.active)
                .ok_or_else(|| failed("source VM status is unavailable"))
        };
        let running = active(&host.vms.statuses(std::slice::from_ref(&p.app)).await)?;
        let snapshot = crate::adapters::vm::snapshot::snapshot_paths(&host.config.snapshot_dir, &p.app);
        let sleeping = !running && snapshot.stamp_path.try_exists().map_err(io_error)?;
        if running || sleeping {
            migration.needs_guest_freeze = true;
            migration.save(dir)?;
        } else if migration.needs_guest_freeze {
            return Err(failed(
                "the source guest must be recovered and frozen before its disk can migrate",
            ));
        }
        if sleeping {
            let record = host
                .state
                .record(&p.app)
                .await
                .ok_or_else(|| failed("sleeping source has no deployment record"))?;
            let slot = host
                .slot_of(&p.app)
                .await
                .ok_or_else(|| failed("sleeping source has no device slot"))?;
            self.shared.attach(&p.volume, &p.app).await?;
            host.vms
                .wake(crate::ports::SuspendRequest {
                    app_id: p.app.clone(),
                    deployment_id: record.deployment_id,
                    slot,
                })
                .await
                .map_err(|e| failed(format!("could not wake the source for migration: {e}")))?;
        }
        let _lease = if running || sleeping {
            let socket = crate::domain::filesystem::reader::guest_vsock_path(host, &p.app);
            let lease = crate::domain::exports::freeze::frozen(&p.app, &socket)
                .await
                .map_err(|e| failed(e.message()))?;
            lease.require_running_guest().map_err(|e| failed(e.message()))?;
            migration.needs_guest_freeze = false;
            migration.save(dir)?;
            Some(lease)
        } else {
            None
        };
        host.vms
            .stop(&p.app)
            .await
            .map_err(|_| failed("could not stop the source guest"))?;
        let states = host.vms.statuses(std::slice::from_ref(&p.app)).await;
        if states.get(&p.app).is_none_or(|vm| vm.active) {
            return Err(failed("source guest is still running"));
        }
        host.vms
            .discard(&p.app)
            .await
            .map_err(|_| failed("could not discard source VM state"))?;
        host.state
            .update_record(&p.app, |record| {
                record.state = protocol::InstanceState::Stopped;
                record.stop_requested = true;
            })
            .await;
        Ok(())
    }

    async fn migrate(&self, host: &Host, p: &Placement) -> Result<(), VolumeError> {
        let dir = p.directory(&self.placements.root);
        let Some(mut migration) = Migration::load(&dir)? else {
            return Ok(());
        };
        if !migration.verified {
            self.stop_source(host, p, &dir, &mut migration).await?;
            self.shared.quiesce_source(&p.volume, &p.app).await?;
            let destination = self.active(p).await?;
            let from = self.shared.filesystem().mount_path.clone();
            let to = destination.filesystem().mount_path.clone();
            let id = p.volume.clone();
            let attempt =
                uuid::Uuid::parse_str(&migration.attempt).map_err(|_| failed("invalid migration attempt"))?;
            tokio::task::spawn_blocking(move || super::transfer::copy_image(&from, &to, &id, attempt))
                .await
                .map_err(|_| failed("migration copy was interrupted"))??;
            destination.flush(&p.app).await?;
            migration.verified = true;
            migration.save(&dir)?;
        }
        let _checkpoints = self.checkpoint_lock.lock().await;
        if migration.retained.is_none() {
            self.active(p)
                .await?
                .verify_migrated_disk(&p.volume, &p.app)
                .await?;
            super::images::remove(&self.shared.filesystem().mount_path, &p.volume)?;
            self.shared.flush(&p.app).await?;
            migration.retained = Some(self.shared.checkpoint_inventory().await?);
            migration.save(&dir)?;
        }
        let inventory = self.shared.checkpoint_inventory().await?;
        let retained = migration
            .retained
            .as_mut()
            .expect("inventory recorded after unlink");
        retained.retain(|name, generation| inventory.get(name) == Some(generation));
        let mut released = Vec::new();
        for (name, generation) in retained.iter() {
            let Ok(id) = protocol::CheckpointId::parse(name) else {
                continue;
            };
            if super::checkpoints::belongs_to(&self.checkpoint_owners, &id, &p.volume, generation)? {
                self.shared.delete_checkpoint(&id).await?;
                super::runtime::remove_file(&self.checkpoint_owners.join(name))?;
                released.push(name.clone());
            }
        }
        for name in released {
            retained.remove(&name);
        }
        if !retained.is_empty() {
            tracing::debug!(app_id = %p.app, remaining = retained.len(), "historical shared checkpoints still retain the source");
            return Ok(());
        }
        std::fs::remove_file(dir.join(JOURNAL)).map_err(io_error)?;
        sync_dir(&dir)?;
        tracing::info!(app_id = %p.app, "customer volume migration and shared checkpoint cleanup complete");
        Ok(())
    }
}
