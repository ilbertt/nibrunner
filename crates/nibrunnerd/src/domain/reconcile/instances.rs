use std::collections::BTreeSet;
use std::sync::Arc;

use protocol::{AppId, DesiredInstance, DesiredInstanceState, InstanceState, StateMessage};

use crate::adapters::vm::{VmStatus, UNKNOWN_VM};
use crate::clock::{now_ms, now_timestamp};
use crate::domain::activation::SleepReason;
use crate::domain::backoff::{is_ready_to_retry, next_attempt_window, BackoffPolicy, NO_START_ATTEMPTS};
use crate::domain::health::{
    apply_probe, describe_instance_failure, evaluate_instance_state, initial_tracker, next_probe_delay_ms,
    LifecycleInputs,
};
use crate::domain::metrics::converge;
use crate::domain::metrics::health::Failure;
use crate::domain::metrics::resources::Operation;
use crate::domain::metrics::sleep_wake::SleepOutcome;
use crate::domain::reconcile::plan::{InstancePlan, ReconcilePlan};
use crate::domain::report::instance_record::{InstanceRecord, RecordFields};
use crate::host::Host;
use crate::ports::{BootRequest, SuspendRequest, VmError, WakeOutcome};

/// The host ports a slot hands this app's extra ports, in the order the document named them.
///
/// Index 0 of the slot is the HTTP port, so these start at 1. A port the slot has no room for is
/// left out rather than folded onto another app's: the document is refused before it reaches here.
fn record_ports(
    desired: &DesiredInstance,
    slot: &nft_render::AppSlot,
) -> Vec<crate::domain::report::instance_record::RecordPort> {
    desired
        .config
        .ports
        .iter()
        .enumerate()
        .filter_map(|(index, port)| {
            Some(crate::domain::report::instance_record::RecordPort {
                name: port.name.clone(),
                host_port: slot.host_port_at(u32::try_from(index).ok()? + 1)?,
                guest_port: port.guest_port,
            })
        })
        .collect()
}

fn record_fields(desired: &DesiredInstance, slot: &nft_render::AppSlot) -> RecordFields {
    RecordFields {
        app_id: desired.app_id.clone(),
        deployment_id: desired.deployment_id.clone(),
        volume_id: desired.volume_id.clone(),
        hostnames: desired.hostnames.clone(),
        host_port: slot.host_port,
        http_port: desired.config.http_port,
        ports: record_ports(desired, slot),
        guest_ipv4: slot.guest_ipv4.clone(),
        layer_digests: desired
            .layers
            .iter()
            .map(|layer| layer.object().digest.clone())
            .collect(),
        health_check: desired.config.health_check.clone(),
        resources: desired.config.resources,
        readiness: desired.activation().ready_when,
        desired_running: true,
        on_request: desired.desired_state == DesiredInstanceState::OnRequest,
    }
}

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
    host.state
        .update_record(app_id, |record| {
            record.state = InstanceState::Stopped;
            record.start_attempts = NO_START_ATTEMPTS;
        })
        .await;
}

pub async fn suspend_instance(host: &Host, app_id: &AppId, why: SleepReason) {
    let reason = why.as_str();
    let Some(record) = host.state.record(app_id).await else {
        return;
    };
    let Some(slot) = host.slot_of(app_id).await else {
        stop_instance(host, app_id, reason).await;
        return;
    };

    host.state.mark_snapshotting(app_id, true).await;
    let started = std::time::Instant::now();
    settled(host, app_id, reason).await;
    let flushed = started.elapsed();
    let outcome = host
        .vms
        .sleep(SuspendRequest {
            app_id: app_id.clone(),
            deployment_id: record.deployment_id.clone(),
            slot,
        })
        .await;
    let snapshotted = started.elapsed() - flushed;
    let outcome = match outcome {
        Ok(()) => {
            host.state
                .update_record(app_id, |record| {
                    record.state = InstanceState::Idle;
                    record.stop_requested = true;
                    record.start_attempts = NO_START_ATTEMPTS;
                    record.message = None;
                })
                .await;
            tracing::info!(
                %app_id,
                reason,
                flushed_ms = flushed.as_millis(),
                snapshotted_ms = snapshotted.as_millis(),
                "app put to sleep"
            );
            SleepOutcome::Slept
        }
        Err(VmError::SleepRefused { reason: refusal }) => {
            tracing::warn!(%app_id, reason, refusal, "this microVM may not be snapshotted, so it stays up");
            SleepOutcome::Refused
        }
        Err(error) => {
            tracing::warn!(%app_id, reason, error = %error.message(), "this microVM would not sleep; leaving it up");
            SleepOutcome::Failed
        }
    };
    host.metrics
        .sleep_wake
        .slept(app_id, why, outcome, flushed, snapshotted);
    host.state.mark_snapshotting(app_id, false).await;
}

fn restarted(existing: Option<&InstanceRecord>) -> bool {
    existing.is_some_and(|record| !record.stop_requested)
}

// A start is refused for two very different reasons, and an instance waiting out its backoff read
// exactly like one that is never starting again: failed, with the message from the last time it
// worked. Only one of them is something an operator can do anything about.
enum StartRefused {
    OutOfRestarts { attempted: u32, allowed: u32 },
    UntilTheBackoffIsDone,
}

fn start_refused(
    existing: Option<&InstanceRecord>,
    now_ms: i64,
    desired: &DesiredInstance,
) -> Option<StartRefused> {
    let existing = existing?;
    let policy = &desired.config.restart_policy;
    if existing.start_attempts.attempts > policy.max_restarts {
        return Some(StartRefused::OutOfRestarts {
            attempted: existing.start_attempts.attempts,
            allowed: policy.max_restarts,
        });
    }
    (!is_ready_to_retry(&existing.start_attempts, now_ms, &BackoffPolicy::from(policy)))
        .then_some(StartRefused::UntilTheBackoffIsDone)
}

// The attempts are what the decision is made on, and restartCount is not that number — it counts
// the starts that worked. So the sentence carries both, because an instance out of restarts is
// otherwise indistinguishable from one that has never been restarted at all.
async fn say_it_is_out_of_restarts(host: &Host, app_id: &AppId, attempted: u32, allowed: u32) {
    let said = StateMessage::new(format!(
        "out of restarts: {attempted} starts attempted against a budget of {allowed},          and this instance will not be started again until it is deployed afresh"
    ));
    if say(host, app_id, said).await {
        host.metrics.health.failed(app_id, Failure::OutOfRestarts);
    }
}

/// Puts a sentence on the record, and says whether it is news. The status loop passes every
/// second, and a record written every second is a write per second that says what the last one
/// did — and a failure counted every second is one failure counted for ever.
async fn say(host: &Host, app_id: &AppId, said: StateMessage) -> bool {
    let mut news = false;
    host.state
        .update_record(app_id, |record| {
            news = record.message.as_ref() != Some(&said);
            if news {
                record.message = Some(said);
            }
        })
        .await;
    news
}

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
    if let Some(refusal) = start_refused(existing.as_ref(), now, desired) {
        if let StartRefused::OutOfRestarts { attempted, allowed } = refusal {
            say_it_is_out_of_restarts(host, &desired.app_id, attempted, allowed).await;
        }
        return;
    }
    let Ok(slot) = host.slot_for(&desired.app_id).await else {
        if say(
            host,
            &desired.app_id,
            StateMessage::new("this host has no slot left"),
        )
        .await
        {
            host.metrics.health.failed(&desired.app_id, Failure::NoSlot);
        }
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
    attempted.stop_requested = false;
    host.state.put_record(attempted.clone()).await;
    host.state.mark_active(&desired.app_id, now).await;

    // Before anything is fetched or attached: a microVM this host could not have put within
    // reach is a cost paid for an app nobody could have reached.
    if let Some(refusal) = crate::domain::reconcile::ingress::ingress_refusal(desired, &host.config) {
        host.state
            .update_record(&desired.app_id, |record| {
                record.state = InstanceState::Failed;
                record.message = Some(StateMessage::new(refusal.clone()));
            })
            .await;
        host.metrics.health.failed(&desired.app_id, Failure::Refused);
        tracing::error!(app_id = %desired.app_id, refusal, "instance refused before it was started");
        return;
    }

    let booted = match host.payloads.prepare(&desired.layers).await {
        Err(error) => Err((Failure::Layers, error.message())),
        Ok(payload) => {
            let attaching = std::time::Instant::now();
            let attached = host.volumes.attach(&desired.volume_id, &desired.app_id).await;
            host.metrics
                .resources
                .done(Operation::VolumeAttach, attached.is_ok(), attaching.elapsed());
            let data_device_path = match attached {
                Ok(attached) => {
                    let attached_at = now_ms();
                    converge::stamp(&host.state, &desired.app_id, |deploy| {
                        deploy.volume_ready_at_ms = Some(attached_at);
                    })
                    .await;
                    attached.device_path
                }
                Err(error) => {
                    host.state
                        .update_record(&desired.app_id, |record| {
                            record.state = InstanceState::Failed;
                            record.message = Some(StateMessage::new(error.message()));
                        })
                        .await;
                    host.metrics.health.failed(&desired.app_id, Failure::Volume);
                    tracing::error!(app_id = %desired.app_id, error = %error.message(), "instance start failed");
                    return;
                }
            };
            host.vms
                .boot(BootRequest {
                    desired: desired.clone(),
                    slot,
                    data_device_path,
                    payload,
                })
                .await
                .map_err(|error| (Failure::Boot, error.message()))
        }
    };

    match booted {
        Ok(()) => {
            let started_at = now_timestamp();
            let booted_at = started_at.epoch_ms();
            host.state
                .update_record(&desired.app_id, |record| {
                    record.started_at = Some(started_at);
                    record.state = InstanceState::Starting;
                    record.health = initial_tracker();
                    record.restart_count += u32::from(restarted(existing.as_ref()));
                    record.message = None;
                })
                .await;
            converge::stamp(&host.state, &desired.app_id, |deploy| {
                deploy.booted_at_ms = Some(booted_at);
            })
            .await;
            host.state.probe_at_once(&desired.app_id).await;
            tracing::info!(app_id = %desired.app_id, host_port = %attempted.host_port, guest_ipv4 = %attempted.guest_ipv4, "instance started");
        }
        Err((failure, reason)) => {
            host.state
                .update_record(&desired.app_id, |record| {
                    record.state = InstanceState::Failed;
                    record.message = Some(StateMessage::new(reason.clone()));
                })
                .await;
            host.metrics.health.failed(&desired.app_id, failure);
            tracing::error!(app_id = %desired.app_id, attempt = attempted.start_attempts.attempts, reason, "instance start failed");
        }
    }
}

pub async fn resume_instance(host: &Host, desired: &DesiredInstance) -> Result<WakeOutcome, String> {
    let Ok(slot) = host.slot_for(&desired.app_id).await else {
        return Err("this host has no slot left".to_string());
    };

    let status = host.vms.statuses(std::slice::from_ref(&desired.app_id)).await;
    if status.get(&desired.app_id).copied().unwrap_or(UNKNOWN_VM).active {
        return Ok(WakeOutcome::AlreadyRunning);
    }

    host.state.mark_active(&desired.app_id, now_ms()).await;

    let request = SuspendRequest {
        app_id: desired.app_id.clone(),
        deployment_id: desired.deployment_id.clone(),
        slot,
    };
    match host.vms.wake(request).await {
        Ok(()) => {
            let started_at = now_timestamp();
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

async fn verdict(
    host: &Host,
    state: InstanceState,
    status: &VmStatus,
    health: &crate::domain::health::HealthTracker,
    record: &InstanceRecord,
) -> Option<StateMessage> {
    if state != InstanceState::Failed {
        return None;
    }
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

pub async fn refresh_states(host: &Arc<Host>) {
    let snapshot = host.state.snapshot().await;
    let app_ids: Vec<AppId> = snapshot.records.keys().cloned().collect();
    let statuses = host.vms.statuses(&app_ids).await;
    let now = now_ms();

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
        let probed = std::time::Instant::now();
        let healthy = if crate::domain::activation::probes_a_port(record.readiness) {
            crate::domain::health::probe::probe_instance(
                &record.guest_ipv4,
                record.http_port,
                &record.health_check,
            )
            .await
        } else {
            // A guest this host did not build answers no port it was never told about, so that
            // its microVM is still up is the whole of what this host can observe about it.
            true
        };
        host.metrics
            .health
            .probed(&record.app_id, healthy, probed.elapsed());
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
    match state {
        InstanceState::Failed if status.active => {
            host.metrics.health.failed(&record.app_id, Failure::NeverAnswered)
        }
        InstanceState::Failed => host.metrics.health.failed(&record.app_id, Failure::Exited),
        InstanceState::Unhealthy => host.metrics.health.went_unhealthy(&record.app_id),
        _ => {}
    }
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

fn starting(plan: &ReconcilePlan) -> impl Iterator<Item = &DesiredInstance> {
    plan.instances.iter().filter_map(|action| match action {
        InstancePlan::Start { desired } | InstancePlan::Replace { desired } => Some(desired),
        _ => None,
    })
}

pub fn layers_to_start(plan: &ReconcilePlan) -> Vec<protocol::DesiredLayer> {
    let mut seen = BTreeSet::new();
    starting(plan)
        .flat_map(|desired| &desired.layers)
        .filter(|layer| seen.insert((*layer).clone()))
        .cloned()
        .collect()
}

pub async fn prefetch_layers(host: &Host, plan: &ReconcilePlan) {
    let mut waiting: Vec<&DesiredInstance> = starting(plan).collect();
    let mut prepared = BTreeSet::new();
    for layer in layers_to_start(plan) {
        let fetching = std::time::Instant::now();
        match host.payloads.prepare(std::slice::from_ref(&layer)).await {
            Ok(payload) if payload.fetched_bytes > 0 => {
                host.metrics
                    .resources
                    .layer_fetched(payload.fetched_bytes, fetching.elapsed());
                prepared.insert(layer);
            }
            Ok(_) => {
                host.metrics.resources.layer_cached();
                prepared.insert(layer);
            }
            Err(error) => {
                host.metrics
                    .resources
                    .done(Operation::LayerFetch, false, fetching.elapsed());
                tracing::warn!(digest = %layer.object().digest, error = %error.message(), "layer prefetch failed");
            }
        }
        let ready_at = now_ms();
        let (ready, still_waiting): (Vec<_>, Vec<_>) = waiting
            .into_iter()
            .partition(|desired| desired.layers.iter().all(|layer| prepared.contains(layer)));
        waiting = still_waiting;
        for desired in ready {
            converge::stamp(&host.state, &desired.app_id, |deploy| {
                deploy.layers_ready_at_ms = Some(ready_at);
            })
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn one_image_is_fetched_per_layer_however_many_apps_deploy_it() {
        let same = layer(|_| {});
        let plan = ReconcilePlan {
            instances: vec![
                InstancePlan::Start {
                    desired: desired_instance(|instance| instance.layers = vec![base_layer(), same.clone()]),
                },
                InstancePlan::Replace {
                    desired: desired_instance(|instance| {
                        instance.app_id = AppId::parse("app-2").unwrap();
                        instance.layers = vec![base_layer(), same.clone()];
                    }),
                },
                InstancePlan::Stop {
                    app_id: app_id(),
                    reason: crate::domain::reconcile::InstanceStopReason::Idle,
                },
                InstancePlan::Sleep {
                    desired: desired_instance(|_| {}),
                },
            ],
            ..Default::default()
        };
        let wanted = layers_to_start(&plan);
        assert_eq!(wanted, vec![base_layer(), same.clone()]);
        let whole = protocol::DesiredLayer::Filesystem {
            object: same.object().clone(),
        };
        let same_bytes_twice = layers_to_start(&ReconcilePlan {
            instances: vec![InstancePlan::Start {
                desired: desired_instance(|instance| instance.layers = vec![whole.clone(), same.clone()]),
            }],
            ..Default::default()
        });
        assert_eq!(
            same_bytes_twice,
            vec![whole, same],
            "one digest as two kinds is two images"
        );
        assert!(layers_to_start(&ReconcilePlan {
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
        assert!(start_refused(None, 0, &desired).is_none());
        let spent = instance_record(|record| {
            record.start_attempts = crate::domain::backoff::AttemptWindow {
                attempts: 3,
                last_attempt_at_ms: Some(0),
            };
        });
        assert!(matches!(
            start_refused(Some(&spent), 0, &desired),
            Some(StartRefused::UntilTheBackoffIsDone)
        ));
        assert!(start_refused(Some(&spent), 10_000, &desired).is_none());
    }

    #[test]
    fn an_instance_out_of_restarts_is_refused_for_that_and_not_for_its_backoff() {
        let desired = desired_instance(|_| {});
        let allowed = desired.config.restart_policy.max_restarts;
        let exhausted = instance_record(|record| {
            record.start_attempts = crate::domain::backoff::AttemptWindow {
                attempts: allowed + 1,
                last_attempt_at_ms: Some(0),
            };
        });
        // Long past any backoff, so the only thing still refusing it is the budget.
        assert!(matches!(
            start_refused(Some(&exhausted), 10_000_000, &desired),
            Some(StartRefused::OutOfRestarts { attempted, allowed: budget })
                if attempted == allowed + 1 && budget == allowed
        ));
    }

    #[tokio::test]
    async fn an_instance_that_will_not_start_again_says_so_rather_than_keeping_its_last_good_message() {
        let host = test_host().await;
        let desired = desired_instance(|_| {});
        let allowed = desired.config.restart_policy.max_restarts;
        host.state
            .put_record(instance_record(|record| {
                record.state = InstanceState::Failed;
                record.start_attempts = crate::domain::backoff::AttemptWindow {
                    attempts: allowed + 1,
                    last_attempt_at_ms: Some(0),
                };
                // What the report used to carry: a sentence from the last time it worked.
                record.message = Some(StateMessage::new("starting the tenant as uid 65534"));
            }))
            .await;

        start_instance(&host, &desired).await;

        let said = host.state.record(&app_id()).await.unwrap().message.unwrap();
        assert!(said.as_str().contains("out of restarts"), "{}", said.as_str());
        assert!(
            said.as_str().contains(&format!("budget of {allowed}")),
            "the budget the decision was made against is missing: {}",
            said.as_str()
        );
        assert!(
            said.as_str()
                .contains(&format!("{} starts attempted", allowed + 1)),
            "the attempts the decision was made on are missing: {}",
            said.as_str()
        );

        start_instance(&host, &desired).await;
        assert_eq!(
            host.metrics.health.of(&app_id()).failures,
            failures(&[("out_of_restarts", 1)]),
            "said once, counted once, however many passes say it again"
        );
    }

    fn failures(counted: &[(&str, u64)]) -> [u64; 8] {
        let mut failures = [0; 8];
        for (index, reason) in [
            "refused",
            "no_slot",
            "layers",
            "volume",
            "boot",
            "exited",
            "never_answered",
            "out_of_restarts",
        ]
        .iter()
        .enumerate()
        {
            failures[index] = counted
                .iter()
                .find(|(each, _)| each == reason)
                .map_or(0, |(_, count)| *count);
        }
        failures
    }

    #[test]
    fn only_a_boot_nobody_asked_for_counts_as_a_restart() {
        assert!(!restarted(None));
        assert!(restarted(Some(&instance_record(|_| {}))));
        assert!(!restarted(Some(&instance_record(|record| record
            .stop_requested =
            true))));
    }

    fn spent_window() -> crate::domain::backoff::AttemptWindow {
        crate::domain::backoff::AttemptWindow {
            attempts: 3,
            last_attempt_at_ms: Some(now_ms()),
        }
    }

    fn cached_image(host: &crate::host::Host) -> std::path::PathBuf {
        crate::adapters::vm::layers::layer_image_path(&host.config.artifact_cache_dir(), &layer(|_| {}))
    }

    fn refuse_artifacts(host: &mut TestHost, reason: &str) {
        let cache_dir = host.config.artifact_cache_dir();
        let refusing = mocks::artifacts_refusing(crate::ports::ArtifactError::Transfer(reason.to_string()));
        Arc::get_mut(&mut host.host)
            .expect("nothing else holds this host yet")
            .payloads = crate::adapters::vm::layers::LayerImages::new(refusing, cache_dir);
    }

    fn on_request() -> DesiredInstance {
        desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest)
    }

    #[tokio::test]
    async fn an_app_this_host_has_never_served_is_left_a_record_waiting_to_be_asked_for() {
        let host = test_host().await;

        sleep_instance(&host, &on_request()).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
        assert!(record.on_request);
        assert!(record.desired_running);
        assert!(host.slot_of(&app_id()).await.is_some());
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn one_this_host_already_holds_takes_the_new_release_without_leaving_the_state_it_is_in() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| {
                record.state = InstanceState::Stopped;
                record.restart_count = 3;
            }))
            .await;
        let newer = desired_instance(|instance| {
            instance.desired_state = DesiredInstanceState::OnRequest;
            instance.deployment_id = protocol::DeploymentId::parse("dep-2").unwrap();
        });

        sleep_instance(&host, &newer).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Stopped);
        assert_eq!(record.deployment_id.as_str(), "dep-2");
        assert_eq!(record.restart_count, 3);
        assert!(record.on_request);
    }

    #[tokio::test]
    async fn a_stop_leaves_the_record_stopped_with_the_attempts_it_spent_given_back() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| record.start_attempts = spent_window()))
            .await;

        stop_instance(&host, &app_id(), "not-desired").await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Stopped);
        assert!(record.stop_requested);
        assert_eq!(record.start_attempts, NO_START_ATTEMPTS);
        assert_eq!(host.vms.calls(), vec![crate::ports::VmCall::Stop]);
    }

    #[tokio::test]
    async fn a_stop_for_an_app_this_host_has_no_record_for_still_takes_the_microvm_down() {
        let host = test_host().await;

        stop_instance(&host, &app_id(), "not-desired").await;

        assert_eq!(host.vms.calls(), vec![crate::ports::VmCall::Stop]);
        assert!(host.state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn an_app_this_host_has_no_record_for_is_not_put_to_sleep() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();

        suspend_instance(&host, &app_id(), SleepReason::Quiet).await;

        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn a_start_inside_the_backoff_it_earned_does_not_touch_the_microvm() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| {
                record.state = InstanceState::Failed;
                record.start_attempts = spent_window();
            }))
            .await;

        start_instance(&host, &desired_instance(|_| {})).await;

        assert!(host.vms.calls().is_empty());
        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Failed
        );
        assert!(host.slot_of(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn an_image_this_host_cannot_fetch_fails_the_instance_rather_than_booting_something_else() {
        let mut host = test_host().await;
        refuse_artifacts(&mut host, "the object store is down");
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();

        start_instance(&host, &desired_instance(|_| {})).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Failed);
        assert!(record
            .message
            .unwrap()
            .as_str()
            .contains("the object store is down"));
        assert!(host.vms.calls().is_empty());
        assert_eq!(
            host.metrics.health.of(&app_id()).failures,
            failures(&[("layers", 1)])
        );
    }

    #[tokio::test]
    async fn a_boot_over_a_microvm_nobody_asked_to_stop_counts_as_a_restart() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state
            .put_record(instance_record(|record| {
                record.restart_count = 1;
                record.stop_requested = false;
            }))
            .await;

        start_instance(&host, &desired_instance(|_| {})).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Starting);
        assert_eq!(record.restart_count, 2);
        assert!(record.started_at.is_some());
        assert!(!record.stop_requested);
        assert_eq!(record.start_attempts.attempts, 1);
    }

    #[tokio::test]
    async fn a_boot_after_a_stop_that_was_asked_for_does_not() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state
            .put_record(instance_record(|record| {
                record.restart_count = 1;
                record.stop_requested = true;
            }))
            .await;

        start_instance(&host, &desired_instance(|_| {})).await;

        assert_eq!(host.state.record(&app_id()).await.unwrap().restart_count, 1);
    }

    #[tokio::test]
    async fn a_wake_that_failed_for_its_own_reason_says_so_rather_than_booting_the_app_cold() {
        let host = test_host().await;
        host.vms
            .refuse_wake(VmError::Host("the snapshot file is unreadable".to_string()));
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
            }))
            .await;

        let refusal = resume_instance(&host, &on_request()).await.unwrap_err();

        assert!(refusal.contains("the snapshot file is unreadable"));
        assert_eq!(host.vms.calls(), vec![crate::ports::VmCall::Wake]);
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
        assert!(record
            .message
            .unwrap()
            .as_str()
            .contains("the snapshot file is unreadable"));
    }

    #[tokio::test]
    async fn the_image_of_everything_the_plan_starts_is_in_the_cache_before_any_of_it_boots() {
        let host = test_host().await;

        prefetch_layers(
            &host,
            &ReconcilePlan {
                instances: vec![InstancePlan::Start {
                    desired: desired_instance(|_| {}),
                }],
                ..Default::default()
            },
        )
        .await;

        assert!(cached_image(&host).exists());
        let page = metrics_page(&host).await;
        assert!(page.contains(
            "nibrunner_storage_operation_seconds_count{operation=\"layer_fetch\",outcome=\"ok\"} 1\n"
        ));
        assert!(page.contains(&format!(
            "nibrunner_layer_fetch_bytes_total {}\n",
            ARTIFACT_BYTES.len()
        )));
        assert!(page.contains("nibrunner_layers_cached_total 0\n"));

        prefetch_layers(
            &host,
            &ReconcilePlan {
                instances: vec![InstancePlan::Start {
                    desired: desired_instance(|_| {}),
                }],
                ..Default::default()
            },
        )
        .await;
        let page = metrics_page(&host).await;
        assert!(
            page.contains("nibrunner_layers_cached_total 1\n"),
            "the second pass found it"
        );
        assert!(page.contains(
            "nibrunner_storage_operation_seconds_count{operation=\"layer_fetch\",outcome=\"ok\"} 1\n"
        ));
    }

    async fn metrics_page(host: &TestHost) -> String {
        crate::domain::metrics::tests::page(
            &crate::domain::metrics::tests::report(),
            &host.metrics,
            &host.state.snapshot().await,
            0,
        )
    }

    #[tokio::test]
    async fn an_image_that_could_not_be_fetched_leaves_the_pass_running() {
        let mut host = test_host().await;
        refuse_artifacts(&mut host, "the object store is down");

        prefetch_layers(
            &host,
            &ReconcilePlan {
                instances: vec![InstancePlan::Start {
                    desired: desired_instance(|_| {}),
                }],
                ..Default::default()
            },
        )
        .await;

        assert!(!cached_image(&host).exists());
        assert!(metrics_page(&host).await.contains(
            "nibrunner_storage_operation_seconds_count{operation=\"layer_fetch\",outcome=\"failed\"} 1\n"
        ));
    }

    #[tokio::test]
    async fn a_microvm_that_went_away_leaves_the_code_it_exited_with_on_the_record() {
        let host = test_host().await;
        host.vms.set_status(VmStatus {
            loaded: true,
            active: false,
            failed: false,
            started_this_boot: true,
            exit_code: Some(137),
        });
        host.state
            .put_record(instance_record(|record| record.started_at = Some(observed_at())))
            .await;

        refresh_states(host.arc()).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Failed);
        assert_eq!(record.last_exit_code, Some(137));
        assert!(record.message.unwrap().as_str().contains("exit code 137"));
        assert_eq!(
            host.metrics.health.of(&app_id()).failures,
            failures(&[("exited", 1)])
        );

        refresh_states(host.arc()).await;
        assert_eq!(
            host.metrics.health.of(&app_id()).failures,
            failures(&[("exited", 1)]),
            "a state that has not moved is not a failure that happened again"
        );
    }

    #[tokio::test]
    async fn a_probe_is_counted_on_the_app_with_what_it_found() {
        let host = test_host().await;
        host.vms.set_status(VmStatus {
            loaded: true,
            active: true,
            failed: false,
            started_this_boot: true,
            exit_code: None,
        });
        host.state
            .put_record(instance_record(|record| {
                record.readiness = protocol::ReadinessPolicy::BootCompleted;
                record.state = InstanceState::Starting;
                record.started_at = Some(now_timestamp());
            }))
            .await;

        refresh_states(host.arc()).await;

        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Running
        );
        let health = host.metrics.health.of(&app_id());
        assert_eq!((health.probes_healthy, health.probes_unhealthy), (1, 0));
    }

    #[tokio::test]
    async fn a_state_that_has_not_moved_keeps_the_message_that_explained_it() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| {
                record.state = InstanceState::Pending;
                record.message = Some(StateMessage::new("waiting for its volume"));
            }))
            .await;

        refresh_states(host.arc()).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Pending);
        assert_eq!(record.message.unwrap().as_str(), "waiting for its volume");
        assert_eq!(record.last_exit_code, None);
    }
}
