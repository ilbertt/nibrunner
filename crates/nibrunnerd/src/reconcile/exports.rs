//! Writing an export, and clearing up after one that did not finish.
//!
//! Ported from `apps/agent/src/services/export-manager.service.ts`.

use protocol::{
    CheckpointId, DesiredExport, ExportId, ExportState, HostDesiredState, ReportedExport, StateMessage,
    Timestamp,
};

use crate::exports::bundle::{dump_volume, write_bundle};
use crate::exports::freeze::frozen;
use crate::exports::reader::ReaderDevice;
use crate::host::Host;
use crate::reconcile::plan::{ExportPlan, ObservedExport, ReconcilePlan};

/// An export's checkpoint is named after the export, so an orphan says who owned it without
/// anything having written that down. That is what a state-driven reap needs: a daemon killed
/// between cutting one and deleting it comes back to a list of names it can recognise, and
/// retrying is the same code path as the first attempt rather than a recovery mode beside it.
const EXPORT_PREFIX: &str = "export-";

pub fn export_checkpoint_id(export_id: &ExportId) -> Option<CheckpointId> {
    CheckpointId::parse(format!("{EXPORT_PREFIX}{export_id}")).ok()
}

pub fn is_export_checkpoint(checkpoint_id: &CheckpointId) -> bool {
    checkpoint_id.as_str().starts_with(EXPORT_PREFIX)
}

/// What this host has already written, from its own note rather than from the store: a bundle is
/// in an object store somebody else owns, and asking it on every pass would be a round trip to
/// re-learn something that cannot change.
pub async fn observe_exports(host: &Host, desired: &HostDesiredState) -> Vec<ObservedExport> {
    let written = host.state.snapshot().await.export_reports;
    desired
        .exports
        .iter()
        .filter(|wanted| {
            written
                .iter()
                .any(|report| report.export_id == wanted.export_id && report.state == ExportState::Ready)
        })
        .map(|wanted| ObservedExport {
            export_id: wanted.export_id.clone(),
            written: true,
        })
        .collect()
}

pub async fn apply_exports(host: &Host, plan: &ReconcilePlan) {
    // Derived from what the store says exists rather than from anything this process remembers,
    // which is what makes retrying the same code path as doing it the first time. Runs on every
    // pass and not only when an export is planned: an orphan is precisely a checkpoint nothing
    // asked for, and while any exists the store pauses segment reclamation for *every* tenant here.
    reap(host, plan).await;

    let mut reports = host.state.snapshot().await.export_reports;
    for action in &plan.exports {
        match action {
            ExportPlan::Write { desired } => {
                let report = write(host, desired).await;
                reports.retain(|held| held.export_id != report.export_id);
                reports.push(report);
            }
            // `Forget`, not delete: expiry is the bucket's lifecycle rule, which is what makes it
            // unforgettable. All this host drops is its own note that it wrote one.
            ExportPlan::Forget { export_id } => reports.retain(|held| &held.export_id != export_id),
            ExportPlan::None { .. } => {}
        }
    }
    host.state
        .modify(|snapshot| snapshot.export_reports = reports)
        .await;
}

/// One export at a time on a host, because the reader is one device: a second export attaching a
/// second checkpoint to it would read the first export's filesystem.
async fn write(host: &Host, desired: &DesiredExport) -> ReportedExport {
    let Some(checkpoint_id) = export_checkpoint_id(&desired.export_id) else {
        return failed(
            desired,
            None,
            "its id does not make a checkpoint name".to_string(),
        );
    };
    let staging_dir = host.config.export_staging_dir.join(desired.export_id.as_str());

    let written = write_inner(host, desired, &checkpoint_id, &staging_dir).await;

    // Whether or not the upload worked: this is a second copy of a tenant's dataset in the clear
    // on a shared host, and the report is written from what came back rather than from what is
    // still lying about.
    let _ = std::fs::remove_dir_all(&staging_dir);

    match written {
        Ok(size_bytes) => {
            tracing::info!(
                export_id = %desired.export_id,
                object_key = %desired.object_key,
                size_bytes,
                "export written"
            );
            ReportedExport {
                export_id: desired.export_id.clone(),
                checkpoint_id: Some(checkpoint_id),
                state: ExportState::Ready,
                size_bytes: Some(size_bytes),
                ready_at: Some(Timestamp::now()),
                message: None,
            }
        }
        Err(reason) => failed(desired, Some(checkpoint_id), reason),
    }
}

async fn write_inner(
    host: &Host,
    desired: &DesiredExport,
    checkpoint_id: &CheckpointId,
    staging_dir: &std::path::Path,
) -> Result<u64, String> {
    // The whole of what a frozen tenant pays for, and the order inside it is the guarantee. The
    // freeze first, because only the guest's kernel can checkpoint the ext4 journal, which the
    // dump never replays — an unfrozen filesystem is missing recent metadata however durable the
    // storage under it is. Then the checkpoint, which is the step that captures *now*.
    // Derived from where this host puts a microVM's working directory rather than asked of the
    // VMM: an app that is not running has no VMM to ask, and that is the ordinary case here.
    let vsock_path = host
        .config
        .vm_dir()
        .join(desired.app_id.as_str())
        .join(guest_contract::vsock::GUEST_VSOCK_FILENAME);
    let lease = frozen(&desired.app_id, &vsock_path)
        .await
        .map_err(|error| error.message())?;
    host.volumes
        .create_checkpoint(checkpoint_id)
        .await
        .map_err(|error| error.message())?;
    // Handed straight back if the guest let go, rather than left for the reap: a cut nobody can
    // vouch for is useless to the export and still pauses reclamation for the whole host.
    if let Err(error) = lease.assert_held() {
        let _ = host.volumes.delete_checkpoint(checkpoint_id).await;
        return Err(error.message());
    }
    drop(lease);
    tracing::info!(
        app_id = %desired.app_id,
        %checkpoint_id,
        "export checkpoint cut while the tenant was frozen"
    );

    let read = read_into_staging(host, desired, checkpoint_id, staging_dir).await;
    // Released as soon as the bytes are out, whichever way the read went: nothing after this
    // reads it, and holding one costs the whole host its storage reclamation.
    if let Err(error) = host.volumes.delete_checkpoint(checkpoint_id).await {
        tracing::error!(
            %checkpoint_id,
            error = %error.message(),
            "export checkpoint not deleted; storage reclamation stays paused"
        );
    }
    read?;

    let bundle = write_bundle(
        &host.artifacts,
        &desired.artifact,
        desired.environment.as_ref(),
        staging_dir,
    )
    .await
    .map_err(|error| error.message())?;
    host.exports
        .upload(&bundle.path, &desired.object_key)
        .await
        .map_err(|error| error.message())?;
    Ok(bundle.size_bytes)
}

/// The server and the device, both taken down in reverse: the device goes first, because taking
/// the socket away from a kernel client that still holds it is how a detach turns into a hang.
async fn read_into_staging(
    host: &Host,
    desired: &DesiredExport,
    checkpoint_id: &CheckpointId,
    staging_dir: &std::path::Path,
) -> Result<(), String> {
    let servers = host
        .checkpoint_servers
        .as_ref()
        .ok_or_else(|| "this host serves no checkpoints".to_string())?;
    let server = servers
        .start(checkpoint_id)
        .await
        .map_err(|error| error.message())?;
    let reader = match ReaderDevice::attach(&host.nbd, server.socket_path(), &desired.volume_id).await {
        Ok(reader) => reader,
        // Two steps rather than one, so an attach that failed still stops the server it started.
        Err(error) => {
            server.stop().await;
            return Err(error.message());
        }
    };
    let dumped = dump_volume(&host.commands, reader.path(), staging_dir).await;
    reader.detach().await;
    server.stop().await;
    dumped.map_err(|error| error.message())
}

/// Cleanup that does not depend on this process having made the mess.
///
/// Nothing in-process covers a daemon killed between cutting a checkpoint and deleting it, and
/// that is the case worth covering — while any checkpoint exists the store pauses segment
/// deletion, compaction and metadata reclamation for *every* tenant on the host.
async fn reap(host: &Host, plan: &ReconcilePlan) {
    // Names an export in flight on this pass would use, which must not be reaped out from under it.
    let in_flight: Vec<CheckpointId> = plan
        .exports
        .iter()
        .filter_map(|action| match action {
            ExportPlan::Write { desired } => export_checkpoint_id(&desired.export_id),
            ExportPlan::Forget { .. } | ExportPlan::None { .. } => None,
        })
        .collect();

    let orphans: Vec<CheckpointId> = host
        .volumes
        .observe_checkpoints()
        .await
        .into_iter()
        .filter(is_export_checkpoint)
        .filter(|held| !in_flight.contains(held))
        .collect();
    if orphans.is_empty() {
        return;
    }
    // A staging tree outlives a kill for the same reason a checkpoint does, and it is a tenant's
    // whole dataset in the clear on a shared host.
    let _ = std::fs::remove_dir_all(&host.config.export_staging_dir);
    let _ = host.nbd.detach(&nft_render::export_reader_device_path()).await;
    for checkpoint_id in &orphans {
        if let Err(error) = host.volumes.delete_checkpoint(checkpoint_id).await {
            tracing::error!(
                %checkpoint_id,
                error = %error.message(),
                "an export checkpoint left behind could not be reaped"
            );
        }
    }
    tracing::warn!(
        checkpoints = orphans.len(),
        "export checkpoints left behind were reaped"
    );
}

fn failed(desired: &DesiredExport, checkpoint_id: Option<CheckpointId>, reason: String) -> ReportedExport {
    tracing::warn!(export_id = %desired.export_id, reason = %reason, "export not written");
    ReportedExport {
        export_id: desired.export_id.clone(),
        checkpoint_id,
        state: ExportState::Failed,
        size_bytes: None,
        ready_at: None,
        message: Some(StateMessage::new(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::plan::plan_reconcile;
    use crate::test_support::*;
    use protocol::{DesiredPresence, ObjectKey, VolumeId};

    fn wanted(state: DesiredPresence) -> DesiredExport {
        DesiredExport {
            export_id: ExportId::parse("exp-1").unwrap(),
            app_id: app_id(),
            volume_id: VolumeId::parse("vol-1").unwrap(),
            object_key: ObjectKey::parse("exports/exp-1/bundle.tar.gz").unwrap(),
            artifact: artifact(|_| {}),
            environment: None,
            desired_state: state,
        }
    }

    async fn asked_for(host: &crate::host::Host, exports: Vec<DesiredExport>) -> ReconcilePlan {
        let desired = desired_state(|state| state.exports = exports);
        host.cache.lock().await.accept(desired.clone());
        plan_reconcile(&desired, &observed_state(|_| {}))
    }

    #[test]
    fn an_orphan_says_who_owned_it_without_anything_having_written_that_down() {
        let export_id = ExportId::parse("exp-1").unwrap();
        let checkpoint_id = export_checkpoint_id(&export_id).unwrap();
        assert_eq!(checkpoint_id.as_str(), "export-exp-1");
        assert!(is_export_checkpoint(&checkpoint_id));
        assert!(!is_export_checkpoint(&CheckpointId::parse("nightly-1").unwrap()));
    }

    /// A host keeping volumes as local files has no pinned view to read, and an export that ran
    /// against the live disk would hand over a filesystem the tenant was writing to.
    #[tokio::test]
    async fn a_host_that_cannot_pin_a_view_reports_the_export_failed_rather_than_reading_the_live_disk() {
        let host = test_host().await;
        let plan = asked_for(host.arc(), vec![wanted(DesiredPresence::Present)]).await;

        apply_exports(host.arc(), &plan).await;

        let reports = host.state.snapshot().await.export_reports;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, ExportState::Failed);
        assert!(host.exports_written().is_empty());
    }

    #[tokio::test]
    async fn forgetting_an_export_drops_the_note_and_nothing_else() {
        let host = test_host().await;
        host.state
            .modify(|snapshot| {
                snapshot.export_reports = vec![ReportedExport {
                    export_id: ExportId::parse("exp-1").unwrap(),
                    checkpoint_id: None,
                    state: ExportState::Ready,
                    size_bytes: Some(10),
                    ready_at: None,
                    message: None,
                }];
            })
            .await;
        let plan = asked_for(host.arc(), vec![wanted(DesiredPresence::Absent)]).await;

        apply_exports(host.arc(), &plan).await;
        assert!(host.state.snapshot().await.export_reports.is_empty());
    }

    /// An export already written is not written again: this host holds write-only credentials on
    /// the store, so re-writing on doubt would re-upload a tenant's whole dataset every restart.
    #[tokio::test]
    async fn an_export_this_host_has_already_written_is_not_written_twice() {
        let host = test_host().await;
        let desired = desired_state(|state| state.exports = vec![wanted(DesiredPresence::Present)]);
        host.state
            .modify(|snapshot| {
                snapshot.export_reports = vec![ReportedExport {
                    export_id: ExportId::parse("exp-1").unwrap(),
                    checkpoint_id: None,
                    state: ExportState::Ready,
                    size_bytes: Some(10),
                    ready_at: None,
                    message: None,
                }];
            })
            .await;

        let observed = observe_exports(host.arc(), &desired).await;
        assert_eq!(observed.len(), 1);
        assert!(observed[0].written);

        let plan = plan_reconcile(
            &desired,
            &observed_state(|state| state.exports = observed.clone()),
        );
        assert!(matches!(plan.exports[0], ExportPlan::None { .. }));
    }
}
