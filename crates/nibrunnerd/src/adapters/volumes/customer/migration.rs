use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use protocol::{AppId, DeploymentId, DesiredInstanceState, DesiredPresence, HostDesiredState, StateMessage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::profile::{failed, io_error, sync_dir, Placement};
use super::runtime::CustomerVolumes;
use crate::adapters::volumes::{VolumeBackend, VolumeError};
use crate::host::Host;

pub(super) const JOURNAL: &str = "migration.json";

#[derive(Default, Deserialize, Serialize)]
enum SourceState {
    Checking,
    // Existing journals may already have stopped the guest or detached the source.
    #[default]
    Fenced,
    Restoring {
        deployment: Option<DeploymentId>,
        reason: StateMessage,
    },
    Failed {
        deployment: Option<DeploymentId>,
        reason: StateMessage,
    },
}

#[derive(Deserialize, Serialize)]
pub(super) struct Migration {
    attempt: String,
    #[serde(default)]
    source: SourceState,
    #[serde(default)]
    request_hash: Option<String>,
    verified: bool,
    // v2026.11.16 could persist a verified copy before checking its NBD attachment.
    #[serde(default)]
    committed: bool,
    #[serde(default)]
    checking_device: bool,
    #[serde(default)]
    discard_candidate: bool,
    #[serde(default)]
    needs_guest_freeze: bool,
    #[serde(default)]
    retained: Option<super::checkpoints::Inventory>,
}

impl Migration {
    pub(super) fn new() -> Self {
        Self {
            attempt: uuid::Uuid::new_v4().to_string(),
            source: SourceState::Checking,
            request_hash: None,
            verified: false,
            committed: false,
            checking_device: false,
            discard_candidate: false,
            needs_guest_freeze: false,
            retained: None,
        }
    }

    pub(super) fn rejected(&self) -> bool {
        self.serves_shared() && matches!(self.source, SourceState::Failed { .. })
    }

    pub(super) fn removing_shared_source(&self) -> bool {
        self.committed && self.retained.is_none()
    }

    pub(super) fn fenced(&self) -> bool {
        self.retained.is_none() && !self.committed && !self.serves_shared()
    }

    pub(super) fn serves_shared(&self) -> bool {
        !self.verified
            && !self.committed
            && self.retained.is_none()
            && matches!(self.source, SourceState::Checking | SourceState::Failed { .. })
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
    pub(super) fn serves_shared(&self, p: &Placement) -> bool {
        Migration::load(&p.directory(&self.placements.root))
            .ok()
            .flatten()
            .is_some_and(|m| m.serves_shared())
    }

    pub(crate) fn source_failure(&self, app: &AppId) -> Option<StateMessage> {
        let p = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .get(app)?
            .clone();
        let migration = Migration::load(&p.directory(&self.placements.root)).ok()??;
        if !migration.serves_shared() {
            return None;
        }
        match migration.source {
            SourceState::Failed { reason, .. } => Some(reason),
            _ => None,
        }
    }

    pub(crate) fn cleanup_message(&self, app: &AppId) -> Option<StateMessage> {
        let p = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .get(app)?
            .clone();
        let migration = Migration::load(&p.directory(&self.placements.root)).ok()??;
        if migration.retained.is_some() {
            Some(StateMessage::new(
                "customer volume active; waiting for historical shared checkpoints to expire",
            ))
        } else if migration.committed {
            Some(StateMessage::new(
                "customer volume active; shared source cleanup pending",
            ))
        } else {
            None
        }
    }

    pub(crate) fn migrating(&self, app: &AppId) -> bool {
        self.placements
            .held
            .read()
            .expect("placement lock")
            .get(app)
            .is_some_and(|p| match Migration::load(&p.directory(&self.placements.root)) {
                Ok(Some(migration)) => migration.fenced(),
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

    pub(crate) async fn resume_migrations(self: &Arc<Self>, host: &Arc<Host>, desired: &HostDesiredState) {
        let placements: Vec<_> = self
            .placements
            .held
            .read()
            .expect("placement lock")
            .values()
            .cloned()
            .collect();
        for p in placements {
            let deployment = desired
                .instances
                .iter()
                .find(|i| i.app_id == p.app)
                .map(|i| i.deployment_id.clone());
            let migration = match Migration::load(&p.directory(&self.placements.root)) {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(error) => {
                    tracing::error!(app_id = %p.app, %error, "cannot resume volume migration");
                    continue;
                }
            };
            if migration.rejected()
                && desired.volumes.iter().any(|v| {
                    v.app_id == p.app
                        && v.volume_id == p.volume
                        && v.storage.is_none()
                        && v.desired_state == DesiredPresence::Present
                })
            {
                if let Some(_guard) = self.try_operation(&p.app) {
                    if let Err(error) = self.withdraw_rejected(&p).await {
                        tracing::warn!(app_id = %p.app, %error, "failed migration withdrawal will retry");
                    }
                }
                continue;
            }
            let mut request = None;
            if !migration.verified && !matches!(migration.source, SourceState::Restoring { .. }) {
                let Some(storage) = desired
                    .volumes
                    .iter()
                    .find(|v| v.app_id == p.app && v.desired_state == DesiredPresence::Present)
                    .and_then(|v| v.storage.as_ref())
                else {
                    if migration.fenced()
                        && desired
                            .volumes
                            .iter()
                            .any(|v| v.app_id == p.app && v.desired_state == DesiredPresence::Absent)
                    {
                        if let Some(_guard) = self.try_operation(&p.app) {
                            let mut cancelled = migration;
                            cancelled.source = SourceState::Restoring {
                                deployment: deployment.clone(),
                                reason: StateMessage::new("migration cancelled by volume deletion"),
                            };
                            if let Err(error) = cancelled.save(&p.directory(&self.placements.root)) {
                                tracing::warn!(app_id = %p.app, %error, "migration cancellation will retry");
                            }
                        }
                    }
                    continue;
                };
                let Ok(config) = p.config(storage) else { continue };
                let hash = hex::encode(Sha256::digest(config.as_bytes()));
                if let SourceState::Failed {
                    deployment: previous, ..
                } = &migration.source
                {
                    if *previous == deployment && migration.request_hash.as_ref() == Some(&hash) {
                        continue;
                    }
                }
                request = Some((config, hash));
            }
            let Some(operation) = self.try_operation(&p.app) else {
                continue;
            };
            let (host, volumes) = (host.clone(), self.clone());
            tokio::spawn(async move {
                let _operation = operation;
                if let Err(error) = volumes.migrate(&host, &p, deployment, request).await {
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

    async fn copy_and_attach(
        &self,
        host: &Host,
        p: &Placement,
        dir: &Path,
        migration: &mut Migration,
        attempt: uuid::Uuid,
    ) -> Result<(), VolumeError> {
        let destination = self.active(p).await?;
        if !migration.checking_device {
            self.stop_source(host, p, dir, migration).await?;
            self.shared.quiesce_source(&p.volume, &p.app).await?;
            let from = self.shared.filesystem().mount_path.clone();
            let to = destination.filesystem().mount_path.clone();
            let id = p.volume.clone();
            tokio::task::spawn_blocking(move || super::transfer::stage_image(&from, &to, &id, attempt))
                .await
                .map_err(|_| failed("migration copy was interrupted"))??;
            destination.flush(&p.app).await?;
            // Verification can acquire the shared slot's NBD device before returning an error.
            migration.checking_device = true;
            migration.save(dir)?;
        }
        destination.verify_migrated_disk(&p.volume, &p.app).await
    }

    async fn restore_source(
        &self,
        p: &Placement,
        dir: &Path,
        migration: &mut Migration,
    ) -> Result<(), VolumeError> {
        let SourceState::Restoring { deployment, reason } = &migration.source else {
            return Ok(());
        };
        if super::images::image_size(&self.shared.filesystem().mount_path, &p.volume)?
            .is_none_or(|bytes| bytes == 0)
        {
            return Err(failed("cannot resume a missing shared source"));
        }
        if migration.checking_device {
            self.backend(p).detach(&p.volume, &p.app).await?;
        }
        migration.source = SourceState::Failed {
            deployment: deployment.clone(),
            reason: reason.clone(),
        };
        migration.checking_device = false;
        migration.discard_candidate = true;
        migration.save(dir)?;
        // The dedicated attachment is gone. An unreachable customer writer cannot hold the source offline.
        let target = self.backend(p);
        let cleanup = async {
            if super::mounted(&target.filesystem().mount_path)? {
                let attempt = uuid::Uuid::parse_str(&migration.attempt)
                    .map_err(|_| failed("invalid migration attempt"))?;
                super::transfer::finish_attempt(&target.filesystem().mount_path, &p.volume, attempt, false)?;
                target.flush(&p.app).await?;
                migration.discard_candidate = false;
                migration.save(dir)?;
            }
            Ok::<(), VolumeError>(())
        }
        .await;
        if let Err(error) = cleanup {
            tracing::warn!(app_id = %p.app, %error, "destination scratch cleanup deferred until retry");
        }
        self.stop_writer(p).await
    }

    async fn migrate(
        &self,
        host: &Host,
        p: &Placement,
        deployment: Option<DeploymentId>,
        request: Option<(String, String)>,
    ) -> Result<(), VolumeError> {
        let dir = p.directory(&self.placements.root);
        let Some(mut migration) = Migration::load(&dir)? else {
            return Ok(());
        };
        let attempt =
            uuid::Uuid::parse_str(&migration.attempt).map_err(|_| failed("invalid migration attempt"))?;
        if !migration.verified && matches!(migration.source, SourceState::Restoring { .. }) {
            return self.restore_source(p, &dir, &mut migration).await;
        }
        if let Some((config, hash)) = request {
            if migration.rejected() {
                self.stop_writer(p).await?;
                let current = dir.join(super::profile::WRITER_CONFIG);
                if std::fs::read_to_string(&current).map_err(io_error)? != config {
                    let mut updated = tempfile::NamedTempFile::new_in(&dir).map_err(io_error)?;
                    updated
                        .write_all(config.as_bytes())
                        .and_then(|()| updated.as_file().sync_all())
                        .map_err(io_error)?;
                    updated.persist(current).map_err(|e| io_error(e.error))?;
                    sync_dir(&dir)?;
                }
            }
            migration.request_hash = Some(hash);
        }
        if migration.serves_shared() {
            let probe = async {
                let writer = self.active(p).await?;
                super::preflight::check(&writer, &p.app).await?;
                if migration.discard_candidate {
                    super::transfer::finish_attempt(
                        &writer.filesystem().mount_path,
                        &p.volume,
                        attempt,
                        false,
                    )?;
                    writer.flush(&p.app).await?;
                }
                Ok::<(), VolumeError>(())
            };
            // systemd may keep restarting a writer with invalid credentials while its mount waits.
            let checked = tokio::time::timeout(std::time::Duration::from_secs(30), probe)
                .await
                .unwrap_or_else(|_| Err(failed("customer writer preflight timed out")));
            migration.source = match checked {
                Ok(()) => SourceState::Fenced,
                Err(error) => {
                    let reason =
                        StateMessage::new(format!("migration failed; serving the shared volume: {error}"));
                    tracing::warn!(app_id = %p.app, %error, "customer writer preflight failed");
                    SourceState::Failed { deployment, reason }
                }
            };
            // Start the offline copy on a subsequent pass, after reconciliation has fenced the source.
            migration.save(&dir)?;
            if migration.rejected() {
                self.stop_writer(p).await?;
            }
            return Ok(());
        }
        if !migration.verified {
            if let Err(error) = self.copy_and_attach(host, p, &dir, &mut migration, attempt).await {
                let reason =
                    StateMessage::new(format!("migration failed; serving the shared volume: {error}"));
                migration.source = SourceState::Restoring { deployment, reason };
                migration.save(&dir)?;
                return self.restore_source(p, &dir, &mut migration).await;
            }
            migration.verified = true;
            migration.committed = true;
            return migration.save(&dir);
        }
        let _checkpoints = self.checkpoint_lock.lock().await;
        if migration.retained.is_none() {
            let destination = self.active(p).await?;
            if !migration.committed {
                destination.verify_migrated_disk(&p.volume, &p.app).await?;
                migration.committed = true;
                return migration.save(&dir);
            }
            super::transfer::finish_attempt(&destination.filesystem().mount_path, &p.volume, attempt, true)?;
            destination.flush(&p.app).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_and_legacy_fenced_journals_never_route_to_shared_by_default() {
        let legacy: Migration = serde_json::from_str(r#"{"attempt":"legacy","verified":false}"#).unwrap();
        assert!(legacy.fenced());
        let dir = tempfile::tempdir().unwrap();
        let mut migration = Migration::new();
        assert!(migration.serves_shared());
        migration.source = SourceState::Failed {
            deployment: None,
            reason: StateMessage::new("test"),
        };
        migration.save(dir.path()).unwrap();
        let mut reopened = Migration::load(dir.path()).unwrap().unwrap();
        assert!(reopened.serves_shared());
        reopened.verified = true;
        assert!(!reopened.serves_shared());
        assert!(reopened.fenced());
        reopened.committed = true;
        assert!(!reopened.serves_shared());
        assert!(!reopened.fenced());
        reopened.retained = Some(Default::default());
        assert!(!reopened.serves_shared());
        assert!(!reopened.fenced());
    }
}
