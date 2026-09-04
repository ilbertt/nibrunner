//! What the planner's verbs actually do to a microVM.

use std::sync::Arc;

use protocol::{AppId, DesiredInstance, DesiredInstanceState, InstanceState, StateMessage};

use crate::backoff::{is_ready_to_retry, next_attempt_window, BackoffPolicy, NO_START_ATTEMPTS};
use crate::clock::{now_ms, now_timestamp};
use crate::health::{
    apply_probe, describe_instance_failure, evaluate_instance_state, initial_tracker, next_probe_delay_ms,
    LifecycleInputs,
};
use crate::host::Host;
use crate::reconcile::plan::{InstancePlan, ReconcilePlan};
use crate::report::instance_record::{InstanceRecord, RecordFields};
use crate::services::{BootRequest, SuspendRequest, VmError, WakeOutcome};
use crate::vm::{VmStatus, UNKNOWN_VM};

fn record_fields(desired: &DesiredInstance, slot: &nft_render::AppSlot) -> RecordFields {
    RecordFields {
        app_id: desired.app_id.clone(),
        deployment_id: desired.deployment_id.clone(),
        volume_id: desired.volume_id.clone(),
        hostnames: desired.hostnames.clone(),
        host_port: slot.host_port,
        http_port: desired.config.http_port,
        has_extra_public_port: Some(desired.config.has_extra_public_port),
        guest_ipv4: slot.guest_ipv4.clone(),
        artifact_digest: desired.artifact.digest.clone(),
        health_check: desired.config.health_check.clone(),
        resources: desired.config.resources,
        desired_running: true,
        on_request: desired.desired_state == DesiredInstanceState::OnRequest,
    }
}

/// The disk brought to a point a cold boot can start from, which is what both ways down need
/// first. A microVM is not asked to shut down: the process is signalled, or the VMM is paused
/// where it stands, and the flush is what makes either survivable.
async fn settled(host: &Host, app_id: &AppId, reason: &str) {
    if let Err(error) = host.volumes.flush().await {
        tracing::warn!(%app_id, reason, error = %error.message(), "stopping a guest whose disk would not flush");
    }
}

pub async fn stop_instance(host: &Host, app_id: &AppId, reason: &str) {
    host.state
        .update_record(app_id, |record| {
            record.state = InstanceState::Stopping;
            record.stop_requested = true;
        })
        .await;
    settled(host, app_id, reason).await;
    match host.vms.stop(app_id).await {
        Ok(()) => tracing::info!(%app_id, reason, "instance stopped"),
        Err(error) => tracing::error!(%app_id, reason, error = %error.message(), "instance stop failed"),
    }
    // The budget goes back with it: a stop that was asked for is not a failed start, and an app
    // suspended while it was struggling to boot would otherwise be one nothing could resume.
    host.state
        .update_record(app_id, |record| {
            record.state = InstanceState::Stopped;
            record.start_attempts = NO_START_ATTEMPTS;
        })
        .await;
}

/// `stop_requested` is written after the snapshot and never before it: a sleep refuses a microVM
/// with a stop in flight, so setting the flag first would refuse every sleep this asked for. It
/// still has to be written afterwards, because it is what tells the health loop that a microVM
/// that is gone is asleep rather than crashed.
///
/// The state stays `running` for the whole of it rather than going to `stopping` the way a stop
/// does. That keeps the forward rule pointing at the guest while it is being captured, which is
/// what the requests still arriving want.
///
/// Neither a refusal nor a VMM that would not take the snapshot is a failure here: the microVM is
/// left running either way and the record untouched, so the app goes on serving and the next
/// measurement tick asks again.
pub async fn suspend_instance(host: &Host, app_id: &AppId, reason: &str) {
    let Some(record) = host.state.record(app_id).await else {
        return;
    };
    let Some(slot) = host.slot_of(app_id).await else {
        // No slot is no tap, no address and no device for a restore to land on, so there is
        // nothing a snapshot could be loaded against. Stopping still reclaims the memory.
        stop_instance(host, app_id, reason).await;
        return;
    };

    host.state.mark_snapshotting(app_id, true).await;
    settled(host, app_id, reason).await;
    let outcome = host
        .vms
        .sleep(SuspendRequest {
            app_id: app_id.clone(),
            deployment_id: record.deployment_id.clone(),
            slot,
        })
        .await;
    match outcome {
        Ok(()) => {
            host.state
                .update_record(app_id, |record| {
                    record.state = InstanceState::Idle;
                    record.stop_requested = true;
                    record.start_attempts = NO_START_ATTEMPTS;
                    record.message = None;
                })
                .await;
        }
        // Loud, and carrying which refusal it was: what reaches here is either a stop that landed
        // in the moment since the records were read, or a refusal about the host itself.
        Err(VmError::SleepRefused { reason: refusal }) => {
            tracing::warn!(%app_id, reason, refusal, "this microVM may not be snapshotted, so it stays up");
        }
        Err(error) => {
            tracing::warn!(%app_id, reason, error = %error.message(), "this microVM would not sleep; leaving it up");
        }
    }
    // Cleared however this ended: an app marked as being captured when nothing is capturing it is
    // one whose next real crash reads as a sleep.
    host.state.mark_snapshotting(app_id, false).await;
}

/// A boot that follows a stop somebody asked for is the instance coming back, not the tenant
/// having gone down: counting it would have an app that was suspended over the weekend read as
/// one that crashed.
fn restarted(existing: Option<&InstanceRecord>) -> bool {
    existing.is_some_and(|record| !record.stop_requested)
}

fn is_startable(existing: Option<&InstanceRecord>, now_ms: i64, desired: &DesiredInstance) -> bool {
    let Some(existing) = existing else {
        return true;
    };
    let policy = &desired.config.restart_policy;
    existing.start_attempts.attempts <= policy.max_restarts
        && is_ready_to_retry(&existing.start_attempts, now_ms, &BackoffPolicy::from(policy))
}

/// An app that should be reachable with no microVM behind it yet. It takes a slot and a record
/// like any other instance, because those are what a request has to find: the slot is the port
/// the proxy is sent to and the activator listens on, and the record is what routing is rendered
/// from. An app nobody has visited is still an app this host answers for.
///
/// The state is left to the health loop, which reads `idle` off the same record. Writing it here
/// would let a reconcile landing mid-boot overwrite a microVM that is on its way up.
pub async fn sleep_instance(host: &Host, desired: &DesiredInstance) {
    let Ok(slot) = host.slot_for(&desired.app_id).await else {
        tracing::error!(app_id = %desired.app_id, "this host has no slot left to answer for the app");
        return;
    };
    let fields = record_fields(desired, &slot);
    let existing = host.state.record(&desired.app_id).await;
    match existing {
        Some(mut record) => {
            record.adopt(fields);
            host.state.put_record(record).await;
        }
        None => {
            host.state
                .put_record(InstanceRecord::new(
                    fields,
                    InstanceState::Idle,
                    initial_tracker(),
                ))
                .await;
            tracing::info!(app_id = %desired.app_id, host_port = %slot.host_port, "app is waiting to be asked for");
        }
    }
}

pub async fn start_instance(host: &Host, desired: &DesiredInstance) {
    let now = now_ms();
    let existing = host.state.record(&desired.app_id).await;
    if !is_startable(existing.as_ref(), now, desired) {
        return;
    }
    let Ok(slot) = host.slot_for(&desired.app_id).await else {
        host.state
            .update_record(&desired.app_id, |record| {
                record.message = Some(StateMessage::new("this host has no slot left"));
            })
            .await;
        return;
    };

    let mut attempted = match existing.clone() {
        Some(record) => record,
        None => InstanceRecord::new(
            record_fields(desired, &slot),
            InstanceState::Pending,
            initial_tracker(),
        ),
    };
    attempted.adopt(record_fields(desired, &slot));
    attempted.start_attempts = next_attempt_window(
        &existing
            .as_ref()
            .map(|record| record.start_attempts)
            .unwrap_or(NO_START_ATTEMPTS),
        now,
        desired.config.restart_policy.reset_after_ms,
    );
    // Whatever asked for the stop has been overtaken by whatever asked for this: leaving the flag
    // set would have a wake that failed read as a sleeping app rather than a broken one.
    attempted.stop_requested = false;
    host.state.put_record(attempted.clone()).await;
    // Before the boot rather than after: the clock this starts is the one that decides when the
    // app may sleep again, and a boot that takes seconds must not spend them.
    host.state.mark_active(&desired.app_id, now).await;

    let artifact = crate::vm::artifacts::ensure_artifact_image(
        &host.artifacts,
        &host.config.artifact_cache_dir(),
        &desired.artifact,
    )
    .await;
    let booted = match artifact {
        Err(error) => Err(error.message()),
        Ok(artifact_image_path) => {
            let data_device_path = match host.volumes.attach(&desired.volume_id, &desired.app_id).await {
                Ok(attached) => attached.device_path,
                Err(error) => {
                    host.state
                        .update_record(&desired.app_id, |record| {
                            record.state = InstanceState::Failed;
                            record.message = Some(StateMessage::new(error.message()));
                        })
                        .await;
                    tracing::error!(app_id = %desired.app_id, error = %error.message(), "instance start failed");
                    return;
                }
            };
            host.vms
                .boot(BootRequest {
                    desired: desired.clone(),
                    slot,
                    data_device_path,
                    artifact_image_path,
                })
                .await
                .map_err(|error| error.message())
        }
    };

    match booted {
        Ok(()) => {
            let started_at = now_timestamp();
            host.state
                .update_record(&desired.app_id, |record| {
                    record.started_at = Some(started_at);
                    record.state = InstanceState::Starting;
                    record.health = initial_tracker();
                    record.restart_count += u32::from(restarted(existing.as_ref()));
                    record.message = None;
                })
                .await;
            host.state.probe_at_once(&desired.app_id).await;
            tracing::info!(app_id = %desired.app_id, host_port = %attempted.host_port, guest_ipv4 = %attempted.guest_ipv4, "instance started");
        }
        Err(reason) => {
            host.state
                .update_record(&desired.app_id, |record| {
                    record.state = InstanceState::Failed;
                    record.message = Some(StateMessage::new(reason.clone()));
                })
                .await;
            tracing::error!(app_id = %desired.app_id, attempt = attempted.start_attempts.attempts, reason, "instance start failed");
        }
    }
}

/// None of the accounting a start does. No attempt is charged to the restart budget and
/// `restart_count` does not move, because a wake is not a restart: an app woken every morning for
/// a year would otherwise report three hundred crashes. The budget still bounds the damage,
/// because the fallback below is a start and a start is what spends it.
///
/// `SnapshotUnusable` is the only failure that boots instead. Every other one leaves the app down
/// with the reason on its record: the wake discards the snapshot on its way out, so the request
/// after this one is the cold boot rather than a second attempt at the same restore.
pub async fn resume_instance(host: &Host, desired: &DesiredInstance) -> Result<WakeOutcome, String> {
    let Ok(slot) = host.slot_for(&desired.app_id).await else {
        return Err("this host has no slot left".to_string());
    };

    // Asked of the process rather than of the record, because the record is a cache and this is a
    // precondition: Firecracker takes `PUT /snapshot/load` only from a process that has configured
    // nothing, and starting one for a microVM already up would have the load reach a booted guest
    // and be refused there.
    let status = host.vms.statuses(std::slice::from_ref(&desired.app_id)).await;
    if status.get(&desired.app_id).copied().unwrap_or(UNKNOWN_VM).active {
        return Ok(WakeOutcome::AlreadyRunning);
    }

    // For the reason a start marks one before its boot: the clock this starts is the one that
    // decides when the app may sleep again.
    host.state.mark_active(&desired.app_id, now_ms()).await;

    let request = SuspendRequest {
        app_id: desired.app_id.clone(),
        deployment_id: desired.deployment_id.clone(),
        slot,
    };
    match host.vms.wake(request).await {
        Ok(()) => {
            let started_at = now_timestamp();
            // The health tracker is carried across rather than reset: it describes the guest
            // being restored and is still true of it, where clearing it would say this app had
            // never answered — the one thing that stops it being allowed to sleep again.
            // `started_at` is not carried across, because the grace period belongs to this run.
            host.state
                .update_record(&desired.app_id, |record| {
                    record.state = InstanceState::Starting;
                    record.started_at = Some(started_at);
                    record.stop_requested = false;
                    record.message = None;
                })
                .await;
            host.state.probe_at_once(&desired.app_id).await;
            Ok(WakeOutcome::Restored)
        }
        Err(VmError::SnapshotUnusable { reason }) => {
            tracing::info!(app_id = %desired.app_id, reason, "nothing to wake this app from; booting it instead");
            start_instance(host, desired).await;
            Ok(WakeOutcome::ColdBoot)
        }
        Err(error) => {
            let reason = error.message();
            host.state
                .update_record(&desired.app_id, |record| {
                    record.message = Some(StateMessage::new(reason.clone()));
                })
                .await;
            Err(reason)
        }
    }
}

/// Only a failure has anything to say: every other state is its own account of itself.
async fn verdict(
    host: &Host,
    state: InstanceState,
    status: &VmStatus,
    health: &crate::health::HealthTracker,
    record: &InstanceRecord,
) -> Option<StateMessage> {
    if state != InstanceState::Failed {
        return None;
    }
    // Only a VM that stopped has left a console to read, and only one this daemon started has a
    // run to read it from.
    let guest_verdict = if status.active || record.started_at.is_none() {
        None
    } else {
        host.vms.guest_verdict(&record.app_id).await
    };
    Some(StateMessage::new(describe_instance_failure(
        status,
        health,
        &record.health_check,
        record.http_port,
        guest_verdict.as_deref(),
    )))
}

/// Probes the tenants that are due, then settles each state from the process and the probe
/// together.
///
/// Every write below merges into the record as it stands rather than replacing it, which is what
/// lets a start landing mid-pass keep the stop it cleared.
pub async fn refresh_states(host: &Arc<Host>) {
    let snapshot = host.state.snapshot().await;
    let app_ids: Vec<AppId> = snapshot.records.keys().cloned().collect();
    let statuses = host.vms.statuses(&app_ids).await;
    let now = now_ms();

    // Concurrently, because what takes any time here is the probe, and sequentially that is one
    // probe ceiling per instance in front of every instance behind it — including the one that
    // has just booted and is waiting to be called `running`.
    let mut settling = Vec::new();
    for record in snapshot.records.values().cloned() {
        let host = host.clone();
        let status = statuses.get(&record.app_id).copied().unwrap_or(UNKNOWN_VM);
        let due = now
            >= snapshot
                .next_probe_at_ms
                .get(&record.app_id)
                .copied()
                .unwrap_or(0);
        let snapshotting = snapshot.snapshotting.contains(&record.app_id);
        settling.push(tokio::spawn(async move {
            settle(&host, record, status, due, snapshotting, now).await;
        }));
    }
    for task in settling {
        let _ = task.await;
    }
}

async fn settle(
    host: &Arc<Host>,
    record: InstanceRecord,
    status: VmStatus,
    due: bool,
    snapshotting: bool,
    now_ms: i64,
) {
    let health = if status.active && due {
        let healthy =
            crate::health::probe::probe_instance(&record.guest_ipv4, record.http_port, &record.health_check)
                .await;
        let delay = next_probe_delay_ms(&record.health, &record.grace_inputs(now_ms));
        host.state
            .modify(|snapshot| {
                snapshot
                    .next_probe_at_ms
                    .insert(record.app_id.clone(), now_ms + delay as i64);
            })
            .await;
        apply_probe(
            &record.health,
            healthy,
            &now_timestamp(),
            record.health_check.healthy_threshold,
        )
    } else {
        record.health.clone()
    };

    let state = evaluate_instance_state(&LifecycleInputs {
        unit: &status,
        tracker: &health,
        health_check: &record.health_check,
        desired_running: record.desired_running,
        on_request: record.on_request,
        stop_requested: record.stop_requested,
        snapshotting,
        started_at_ms: record.started_at.as_ref().map(protocol::Timestamp::epoch_ms),
        now_ms,
        current: record.state,
    });

    if state == record.state {
        host.state
            .update_record(&record.app_id, |latest| latest.health = health)
            .await;
        return;
    }
    // Cleared as readily as it is written: a message outliving the state it explains is read as
    // an account of the state that replaced it.
    let message = verdict(host, state, &status, &health, &record).await;
    host.state
        .update_record(&record.app_id, |latest| {
            latest.health = health;
            latest.state = state;
            if let Some(code) = status.exit_code {
                if !status.active {
                    latest.last_exit_code = Some(code);
                }
            }
            latest.message = message;
        })
        .await;
    let answered_ms = (state == InstanceState::Running)
        .then(|| record.started_at.as_ref().map(|at| now_ms - at.epoch_ms()))
        .flatten();
    tracing::info!(
        app_id = %record.app_id,
        from = record.state.as_str(),
        to = state.as_str(),
        answered_ms,
        "instance state changed"
    );
    host.state.signal_report();
}

/// Which digests a pass is about to need, one entry per digest: two apps deploying the same bytes
/// share the image, and fetching it twice at once would have them race for the same staging path.
pub fn artifacts_to_start(plan: &ReconcilePlan) -> Vec<protocol::DesiredArtifact> {
    let mut seen = std::collections::BTreeSet::new();
    plan.instances
        .iter()
        .filter_map(|action| match action {
            InstancePlan::Start { desired } | InstancePlan::Replace { desired } => Some(&desired.artifact),
            _ => None,
        })
        .filter(|artifact| seen.insert(artifact.digest.clone()))
        .cloned()
        .collect()
}

/// Downloading the artifact and building its image is the longest step of a deploy, and it does
/// not need the host to have stopped anything — so it runs while the outgoing microVM is still
/// serving rather than after it is gone.
///
/// Best-effort: the start asks for the same image and is the one that reports a fetch that could
/// not be made, so a failure here is only a head start that was not taken.
pub async fn prefetch_artifacts(host: &Host, plan: &ReconcilePlan) {
    for artifact in artifacts_to_start(plan) {
        if let Err(error) = crate::vm::artifacts::ensure_artifact_image(
            &host.artifacts,
            &host.config.artifact_cache_dir(),
            &artifact,
        )
        .await
        {
            tracing::warn!(digest = %artifact.digest, error = %error.message(), "artifact prefetch failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn one_image_is_fetched_per_digest_however_many_apps_deploy_it() {
        let same = artifact(|_| {});
        let plan = ReconcilePlan {
            instances: vec![
                InstancePlan::Start {
                    desired: desired_instance(|_| {}),
                },
                InstancePlan::Replace {
                    desired: desired_instance(|instance| {
                        instance.app_id = AppId::parse("app-2").unwrap();
                        instance.artifact = same.clone();
                    }),
                },
                InstancePlan::Stop {
                    app_id: app_id(),
                    reason: crate::reconcile::InstanceStopReason::Idle,
                },
                InstancePlan::Sleep {
                    desired: desired_instance(|_| {}),
                },
            ],
            ..Default::default()
        };
        let wanted = artifacts_to_start(&plan);
        assert_eq!(wanted.len(), 1);
        assert_eq!(wanted[0].digest, same.digest);
        assert!(artifacts_to_start(&ReconcilePlan {
            instances: vec![InstancePlan::Sleep {
                desired: desired_instance(|_| {})
            }],
            ..Default::default()
        })
        .is_empty());
    }

    #[test]
    fn a_start_is_refused_while_its_backoff_still_has_time_to_run() {
        let desired = desired_instance(|_| {});
        assert!(is_startable(None, 0, &desired));
        let spent = instance_record(|record| {
            record.start_attempts = crate::backoff::AttemptWindow {
                attempts: 3,
                last_attempt_at_ms: Some(0),
            };
        });
        assert!(!is_startable(Some(&spent), 0, &desired));
        assert!(is_startable(Some(&spent), 10_000, &desired));
        // Past the budget nothing is startable, however long ago the last attempt was.
        let exhausted = instance_record(|record| {
            record.start_attempts = crate::backoff::AttemptWindow {
                attempts: desired.config.restart_policy.max_restarts + 1,
                last_attempt_at_ms: Some(0),
            };
        });
        assert!(!is_startable(Some(&exhausted), 10_000_000, &desired));
    }

    #[test]
    fn only_a_boot_nobody_asked_for_counts_as_a_restart() {
        assert!(!restarted(None));
        assert!(restarted(Some(&instance_record(|_| {}))));
        assert!(!restarted(Some(&instance_record(|record| record
            .stop_requested =
            true))));
    }
}
