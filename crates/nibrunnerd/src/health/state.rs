use protocol::{HealthCheck, HttpPort, InstanceState, Timestamp};
use serde::{Deserialize, Serialize};

use crate::vm::VmStatus;

/// How often a tenant that has never answered is asked again.
///
/// `interval_ms` is a liveness cadence for something already serving, and spending it on a boot
/// means the gap between a binary starting to listen and a deploy being called done is most of
/// that interval. This is the grid a first answer lands on instead.
pub const STARTUP_PROBE_INTERVAL_MS: u64 = 250;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthTracker {
    pub consecutive_successes: u32,
    pub consecutive_failures: u32,
    pub ever_healthy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_healthy_at: Option<Timestamp>,
}

pub fn initial_tracker() -> HealthTracker {
    HealthTracker::default()
}

/// `ever_healthy` flips at the threshold, not at the first accepted connection: it is what later
/// decides whether a failure run reads as `unhealthy` or as an app that never served at all.
pub fn apply_probe(
    tracker: &HealthTracker,
    healthy: bool,
    at: &Timestamp,
    healthy_threshold: u32,
) -> HealthTracker {
    if !healthy {
        return HealthTracker {
            consecutive_successes: 0,
            consecutive_failures: tracker.consecutive_failures + 1,
            ..tracker.clone()
        };
    }
    let consecutive_successes = tracker.consecutive_successes + 1;
    HealthTracker {
        consecutive_successes,
        consecutive_failures: 0,
        ever_healthy: tracker.ever_healthy || consecutive_successes >= healthy_threshold,
        last_healthy_at: Some(at.clone()),
    }
}

#[derive(Debug, Clone)]
pub struct GraceInputs<'a> {
    pub health_check: &'a HealthCheck,
    pub started_at_ms: Option<i64>,
    pub now_ms: i64,
}

/// An instance with no start time has not been booted by this daemon, so nothing has run out yet.
pub fn is_within_grace_period(grace: &GraceInputs<'_>) -> bool {
    match grace.started_at_ms {
        None => true,
        Some(started_at_ms) => grace.now_ms - started_at_ms < grace.health_check.grace_period_ms as i64,
    }
}

/// Whether this tenant is still being given its first chance to answer. Bounded by the grace
/// period rather than by the state alone, which is what keeps it — and the loop that ticks for
/// it — from running for as long as an app that never settles is up.
pub fn is_on_startup_grid(tracker: &HealthTracker, grace: &GraceInputs<'_>) -> bool {
    !tracker.ever_healthy && is_within_grace_period(grace)
}

/// The fast grid applies only while a tenant is still owed its grace period, so a slow starter is
/// failed on exactly the schedule it was before.
pub fn next_probe_delay_ms(tracker: &HealthTracker, grace: &GraceInputs<'_>) -> u64 {
    if is_on_startup_grid(tracker, grace) {
        STARTUP_PROBE_INTERVAL_MS.min(grace.health_check.interval_ms)
    } else {
        grace.health_check.interval_ms
    }
}

/// Why a `failed` verdict was reached, for the owner who only ever sees the verdict.
///
/// There are two ways to reach it and one fact tells them apart: an instance either stopped when
/// nothing had asked it to, or was still up and never answered. A guest that stopped has usually
/// said why on its console, and that account wins: the exit code beside it is the one *Firecracker*
/// ended with, which is 0 whenever the guest powered itself off deliberately — so on the failure
/// the owner is most likely to hit, it reads as success.
pub fn describe_instance_failure(
    unit: &VmStatus,
    tracker: &HealthTracker,
    health_check: &HealthCheck,
    http_port: HttpPort,
    guest_verdict: Option<&str>,
) -> String {
    if !unit.active {
        if let Some(verdict) = guest_verdict {
            return verdict.to_string();
        }
        return match unit.exit_code {
            None => "the microVM stopped without being asked to".to_string(),
            Some(code) => format!("the microVM stopped without being asked to, exit code {code}"),
        };
    }
    format!(
        "nothing answered on port {http_port} inside the guest: {} health probes failed after the {}ms grace period",
        tracker.consecutive_failures, health_check.grace_period_ms
    )
}

#[derive(Debug, Clone)]
pub struct LifecycleInputs<'a> {
    pub unit: &'a VmStatus,
    pub tracker: &'a HealthTracker,
    pub health_check: &'a HealthCheck,
    pub desired_running: bool,
    pub on_request: bool,
    pub stop_requested: bool,
    pub snapshotting: bool,
    pub started_at_ms: Option<i64>,
    pub now_ms: i64,
    /// What the record already says, which is the only thing that tells a start in flight from an
    /// app waiting to be asked for: both are `on-request` instances with no microVM behind them
    /// yet, and the planner has already decided which by writing `pending` or `idle`.
    pub current: InstanceState,
}

/// What a microVM that is not up means, which is four different things.
///
/// `stopped` and `idle` are the same absence read against who is waiting: one is the end of the
/// release, the other is the release between requests. `pending` is a start still in flight, and
/// only a start this daemon saw through records a time.
///
/// A boot that did happen rules out `idle` whatever the activation policy says: a microVM a
/// request brought up and that then went down unasked is a crash, and calling that idle would
/// wait for another request to find out.
///
/// Unasked is the whole of it, and a snapshot in flight is the one absence that is asked for
/// without `stop_requested` saying so: the capture ends with the VMM gone and the flag is only
/// written once it has finished.
fn evaluate_stopped_state(inputs: &LifecycleInputs<'_>) -> InstanceState {
    let down = if inputs.on_request && inputs.desired_running {
        InstanceState::Idle
    } else {
        InstanceState::Stopped
    };
    if inputs.stop_requested || inputs.snapshotting || !inputs.desired_running {
        return down;
    }
    if inputs.started_at_ms.is_some() {
        return InstanceState::Failed;
    }
    // A start this daemon asked for and has not seen through is not an app waiting to be asked
    // for. Reading a release that is coming up as `idle` tells the control plane it is as up as
    // it will ever get: the startup deadline stops, the deployment turns `running` before a probe
    // has run, and the memory the boot is about to take is left out of what this host counts as
    // committed.
    if inputs.on_request && inputs.current != InstanceState::Pending {
        down
    } else {
        InstanceState::Pending
    }
}

/// A booted microVM is not a running app: `starting` has not accepted a connection and `running`
/// has, and collapsing the two would let a deploy swap traffic onto a booted-but-dead VM.
///
/// A VM that exited unasked is `failed` and never restarted here — the guest owns the tenant's
/// restart budget, and whether to try elsewhere is the reconciler's call.
pub fn evaluate_instance_state(inputs: &LifecycleInputs<'_>) -> InstanceState {
    if inputs.unit.failed {
        return InstanceState::Failed;
    }
    if !inputs.unit.active {
        return evaluate_stopped_state(inputs);
    }
    if inputs.stop_requested {
        return InstanceState::Stopping;
    }
    if inputs.tracker.consecutive_successes >= inputs.health_check.healthy_threshold {
        return InstanceState::Running;
    }
    let within_grace = is_within_grace_period(&GraceInputs {
        health_check: inputs.health_check,
        started_at_ms: inputs.started_at_ms,
        now_ms: inputs.now_ms,
    });
    if inputs.tracker.consecutive_failures >= inputs.health_check.unhealthy_threshold && !within_grace {
        return if inputs.tracker.ever_healthy {
            InstanceState::Unhealthy
        } else {
            InstanceState::Failed
        };
    }
    if inputs.tracker.ever_healthy {
        InstanceState::Running
    } else {
        InstanceState::Starting
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::UNKNOWN_VM;
    use protocol::{DEFAULT_HEALTH_CHECK, DEFAULT_HTTP_PORT};

    const STARTED_AT_MS: i64 = 1_000_000;
    const OBSERVED_AT: &str = "2026-08-03T10:00:00.000Z";

    fn grace_ms() -> i64 {
        DEFAULT_HEALTH_CHECK.grace_period_ms as i64
    }

    fn within_grace() -> i64 {
        STARTED_AT_MS + grace_ms() - 1
    }

    fn past_grace() -> i64 {
        STARTED_AT_MS + grace_ms() + 1
    }

    fn observed_at() -> Timestamp {
        Timestamp::parse(OBSERVED_AT).unwrap()
    }

    fn active() -> VmStatus {
        VmStatus {
            loaded: true,
            active: true,
            failed: false,
            started_this_boot: true,
            exit_code: None,
        }
    }

    fn exited() -> VmStatus {
        VmStatus {
            loaded: true,
            active: false,
            failed: false,
            started_this_boot: true,
            exit_code: Some(0),
        }
    }

    fn crashed() -> VmStatus {
        VmStatus {
            loaded: true,
            active: false,
            failed: true,
            started_this_boot: true,
            exit_code: Some(1),
        }
    }

    fn absent() -> VmStatus {
        UNKNOWN_VM
    }

    fn probe(tracker: &HealthTracker, healthy: bool, healthy_threshold: u32) -> HealthTracker {
        apply_probe(tracker, healthy, &observed_at(), healthy_threshold)
    }

    fn failing(count: u32) -> HealthTracker {
        let mut tracker = initial_tracker();
        for _ in 0..count {
            tracker = probe(&tracker, false, 1);
        }
        tracker
    }

    fn healthy_then(failures: u32) -> HealthTracker {
        let mut tracker = probe(&initial_tracker(), true, 1);
        for _ in 0..failures {
            tracker = probe(&tracker, false, 1);
        }
        tracker
    }

    struct Evaluate {
        unit: VmStatus,
        tracker: HealthTracker,
        now_ms: i64,
        health_check: HealthCheck,
        stop_requested: bool,
        desired_running: bool,
        on_request: bool,
        snapshotting: bool,
        started_at_ms: Option<i64>,
        current: InstanceState,
    }

    impl Default for Evaluate {
        fn default() -> Self {
            Self {
                unit: active(),
                tracker: initial_tracker(),
                now_ms: within_grace(),
                health_check: DEFAULT_HEALTH_CHECK,
                stop_requested: false,
                desired_running: true,
                on_request: false,
                snapshotting: false,
                started_at_ms: Some(STARTED_AT_MS),
                current: InstanceState::Pending,
            }
        }
    }

    fn evaluate(inputs: Evaluate) -> InstanceState {
        evaluate_instance_state(&LifecycleInputs {
            unit: &inputs.unit,
            tracker: &inputs.tracker,
            health_check: &inputs.health_check,
            desired_running: inputs.desired_running,
            on_request: inputs.on_request,
            stop_requested: inputs.stop_requested,
            snapshotting: inputs.snapshotting,
            started_at_ms: inputs.started_at_ms,
            now_ms: inputs.now_ms,
            current: inputs.current,
        })
    }

    fn delay(tracker: &HealthTracker, now_ms: i64, health_check: &HealthCheck) -> u64 {
        next_probe_delay_ms(
            tracker,
            &GraceInputs {
                health_check,
                started_at_ms: Some(STARTED_AT_MS),
                now_ms,
            },
        )
    }

    #[test]
    fn how_soon_a_tenant_is_asked_again() {
        assert_eq!(
            delay(&initial_tracker(), within_grace(), &DEFAULT_HEALTH_CHECK),
            STARTUP_PROBE_INTERVAL_MS
        );
        assert_eq!(
            delay(&healthy_then(0), within_grace(), &DEFAULT_HEALTH_CHECK),
            DEFAULT_HEALTH_CHECK.interval_ms
        );
        assert_eq!(
            delay(&initial_tracker(), past_grace(), &DEFAULT_HEALTH_CHECK),
            DEFAULT_HEALTH_CHECK.interval_ms
        );
        let fast = HealthCheck {
            interval_ms: STARTUP_PROBE_INTERVAL_MS - 1,
            ..DEFAULT_HEALTH_CHECK
        };
        assert_eq!(delay(&initial_tracker(), within_grace(), &fast), fast.interval_ms);
    }

    #[test]
    fn probe_accounting_keeps_the_fact_it_was_once_healthy() {
        let tracker = probe(&failing(2), true, 1);
        assert_eq!(tracker.consecutive_successes, 1);
        assert_eq!(tracker.consecutive_failures, 0);
        assert!(tracker.ever_healthy);
        assert_eq!(tracker.last_healthy_at, Some(observed_at()));
        let then_failed = probe(&tracker, false, 1);
        assert_eq!(then_failed.consecutive_successes, 0);
        assert_eq!(then_failed.consecutive_failures, 1);
        assert!(then_failed.ever_healthy);
    }

    #[test]
    fn a_booted_vm_is_not_a_running_app() {
        assert_eq!(evaluate(Evaluate::default()), InstanceState::Starting);
        assert_eq!(
            evaluate(Evaluate {
                tracker: probe(&initial_tracker(), true, 1),
                ..Default::default()
            }),
            InstanceState::Running
        );
        assert_eq!(
            evaluate(Evaluate {
                tracker: failing(10),
                ..Default::default()
            }),
            InstanceState::Starting
        );
        assert_eq!(
            evaluate(Evaluate {
                tracker: failing(DEFAULT_HEALTH_CHECK.unhealthy_threshold),
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Failed
        );
        assert_eq!(
            evaluate(Evaluate {
                tracker: healthy_then(DEFAULT_HEALTH_CHECK.unhealthy_threshold),
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Unhealthy
        );
        assert_eq!(
            evaluate(Evaluate {
                tracker: healthy_then(1),
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Running
        );
    }

    #[test]
    fn what_the_vm_itself_is_doing() {
        assert_eq!(
            evaluate(Evaluate {
                unit: exited(),
                tracker: healthy_then(0),
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Failed
        );
        assert_eq!(
            evaluate(Evaluate {
                unit: crashed(),
                tracker: probe(&initial_tracker(), true, 1),
                ..Default::default()
            }),
            InstanceState::Failed
        );
        assert_eq!(
            evaluate(Evaluate {
                unit: exited(),
                now_ms: past_grace(),
                stop_requested: true,
                ..Default::default()
            }),
            InstanceState::Stopped
        );
        assert_eq!(
            evaluate(Evaluate {
                tracker: probe(&initial_tracker(), true, 1),
                now_ms: past_grace(),
                stop_requested: true,
                ..Default::default()
            }),
            InstanceState::Stopping
        );
        // A start still being staged is pending, though the replaced VM is still on record.
        assert_eq!(
            evaluate(Evaluate {
                unit: exited(),
                now_ms: STARTED_AT_MS,
                started_at_ms: None,
                ..Default::default()
            }),
            InstanceState::Pending
        );
        assert_eq!(
            evaluate(Evaluate {
                unit: absent(),
                now_ms: STARTED_AT_MS,
                started_at_ms: None,
                ..Default::default()
            }),
            InstanceState::Pending
        );
        assert_eq!(
            evaluate(Evaluate {
                unit: absent(),
                now_ms: past_grace(),
                desired_running: false,
                ..Default::default()
            }),
            InstanceState::Stopped
        );
    }

    #[test]
    fn an_app_that_runs_on_request_is_idle_rather_than_stopped() {
        let base = Evaluate {
            on_request: true,
            started_at_ms: None,
            now_ms: STARTED_AT_MS,
            ..Default::default()
        };
        assert_eq!(
            evaluate(Evaluate {
                unit: absent(),
                current: InstanceState::Idle,
                ..base
            }),
            InstanceState::Idle
        );
        // The two look identical from the unit alone; the record is what tells them apart.
        assert_eq!(
            evaluate(Evaluate {
                unit: absent(),
                current: InstanceState::Pending,
                on_request: true,
                started_at_ms: None,
                now_ms: STARTED_AT_MS,
                ..Default::default()
            }),
            InstanceState::Pending
        );
        assert_eq!(
            evaluate(Evaluate {
                unit: exited(),
                on_request: true,
                stop_requested: true,
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Idle
        );
        assert_eq!(
            evaluate(Evaluate {
                unit: exited(),
                on_request: true,
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Failed
        );
        // The snapshot is what takes the VMM down, and `stop_requested` is only written once it
        // has been taken: reading that as a crash fails the deployment.
        assert_eq!(
            evaluate(Evaluate {
                unit: exited(),
                on_request: true,
                snapshotting: true,
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Idle
        );
        assert_eq!(
            evaluate(Evaluate {
                unit: absent(),
                on_request: true,
                desired_running: false,
                now_ms: past_grace(),
                ..Default::default()
            }),
            InstanceState::Stopped
        );
    }

    #[test]
    fn thresholds_are_honoured() {
        let two = HealthCheck {
            healthy_threshold: 2,
            ..DEFAULT_HEALTH_CHECK
        };
        let once = probe(&initial_tracker(), true, 2);
        assert_eq!(
            evaluate(Evaluate {
                tracker: once.clone(),
                health_check: two.clone(),
                ..Default::default()
            }),
            InstanceState::Starting
        );
        let twice = probe(&once, true, 2);
        assert_eq!(
            evaluate(Evaluate {
                tracker: twice,
                health_check: two,
                ..Default::default()
            }),
            InstanceState::Running
        );
    }

    #[test]
    fn a_failure_accounts_for_itself() {
        let failure = |unit: &VmStatus, tracker: &HealthTracker, verdict: Option<&str>| {
            describe_instance_failure(unit, tracker, &DEFAULT_HEALTH_CHECK, DEFAULT_HTTP_PORT, verdict)
        };
        assert_eq!(
            failure(&exited(), &initial_tracker(), None),
            "the microVM stopped without being asked to, exit code 0"
        );
        assert_eq!(
            failure(
                &VmStatus {
                    exit_code: None,
                    ..crashed()
                },
                &initial_tracker(),
                None
            ),
            "the microVM stopped without being asked to"
        );
        // The exit code beside a stopped VM is Firecracker's, and a guest that powered itself off
        // deliberately leaves it 0 — so an owner reading it is told the failure succeeded.
        assert_eq!(
            failure(
                &exited(),
                &initial_tracker(),
                Some("the tenant used its 5 restarts without staying up; shutting the guest down")
            ),
            "the tenant used its 5 restarts without staying up; shutting the guest down"
        );
        let unreachable = HealthTracker {
            consecutive_failures: DEFAULT_HEALTH_CHECK.unhealthy_threshold,
            ..initial_tracker()
        };
        let expected = format!(
            "nothing answered on port {DEFAULT_HTTP_PORT} inside the guest: {} health probes failed after the {}ms grace period",
            DEFAULT_HEALTH_CHECK.unhealthy_threshold, DEFAULT_HEALTH_CHECK.grace_period_ms
        );
        // A VM still up never stopped, so there is no console verdict to prefer.
        assert_eq!(
            failure(
                &active(),
                &unreachable,
                Some("the tenant has stopped; shutting the guest down")
            ),
            expected
        );
        assert_eq!(failure(&active(), &unreachable, None), expected);
    }
}
