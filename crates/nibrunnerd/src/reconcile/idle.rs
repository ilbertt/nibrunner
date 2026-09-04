//! When an app that runs on request has gone quiet enough to sleep.

use protocol::InstanceState;

use crate::report::InstanceRecord;

/// What an `on-request` app is given when the control plane names no timeout of its own — an api
/// that predates the field, or one that could not read it. Long enough not to stop an app out
/// from under somebody reading a page, short enough that most of a quiet day is reclaimed.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Only a microVM that is up can be put down, and only one that is not on its way up.
///
/// `unhealthy` is not among them, though it is up: an app failing its probes has gone quiet
/// *because* it is broken, so the silence is a symptom of the fault rather than evidence nobody
/// wants it. Sleeping it would turn a failure the owner can see into one they cannot, and take it
/// out of the restart and backoff machinery that exists to answer exactly this.
const SLEEPABLE_STATES: [InstanceState; 1] = [InstanceState::Running];

/// An app with no activity recorded is never quiet: it is one this daemon has not watched long
/// enough to say anything about, and the first counter reading gives it a starting point. Erring
/// towards awake is the only safe direction — the cost is memory, and the cost of the other
/// mistake is somebody's request meeting a microVM being shut down underneath it.
pub fn has_gone_quiet(
    record: &InstanceRecord,
    timeout_ms: Option<u64>,
    last_active_at_ms: Option<i64>,
    now_ms: i64,
) -> bool {
    let (Some(timeout_ms), Some(last_active_at_ms)) = (timeout_ms, last_active_at_ms) else {
        return false;
    };
    record.on_request
        && record.desired_running
        && SLEEPABLE_STATES.contains(&record.state)
        && now_ms - last_active_at_ms >= timeout_ms as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::instance_record;
    use protocol::INSTANCE_STATES;

    const TIMEOUT_MS: u64 = 900_000;
    const NOW_MS: i64 = 10_000_000;
    const QUIET_SINCE_MS: i64 = NOW_MS - TIMEOUT_MS as i64;
    const BUSY_SINCE_MS: i64 = QUIET_SINCE_MS + 1;

    fn quiet(record: &InstanceRecord, timeout_ms: Option<u64>, last_active_at_ms: Option<i64>) -> bool {
        has_gone_quiet(record, timeout_ms, last_active_at_ms, NOW_MS)
    }

    fn on_request(state: InstanceState) -> InstanceRecord {
        instance_record(|record| {
            record.on_request = true;
            record.state = state;
        })
    }

    #[test]
    fn a_quiet_serving_app_has_gone_quiet_and_one_asked_for_a_moment_ago_has_not() {
        assert!(quiet(&on_request(InstanceState::Running), Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)));
        assert!(!quiet(&on_request(InstanceState::Running), Some(TIMEOUT_MS), Some(BUSY_SINCE_MS)));
    }

    /// An app failing its probes has gone quiet because it is broken, so the silence is a symptom
    /// of the fault rather than evidence nobody wants it.
    #[test]
    fn an_unhealthy_app_is_not_a_quiet_one_however_long_nobody_has_asked_for_it() {
        assert!(!quiet(&on_request(InstanceState::Unhealthy), Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)));
    }

    #[test]
    fn an_app_that_is_kept_up_or_suspended_is_never_let_go() {
        let always_up = instance_record(|record| {
            record.on_request = false;
            record.state = InstanceState::Running;
        });
        assert!(!quiet(&always_up, Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)));
        let suspended = instance_record(|record| {
            record.on_request = true;
            record.state = InstanceState::Running;
            record.desired_running = false;
        });
        assert!(!quiet(&suspended, Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)));
    }

    #[test]
    fn every_other_state_has_no_microvm_to_stop() {
        for state in INSTANCE_STATES
            .iter()
            .filter(|state| **state != InstanceState::Running && **state != InstanceState::Unhealthy)
        {
            assert!(!quiet(&on_request(*state), Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)), "{state:?}");
        }
    }

    /// The two ways of knowing nothing. A restarted daemon has watched no traffic yet, and an app
    /// desired state no longer calls `on-request` has no timeout to be measured against.
    #[test]
    fn an_app_nothing_has_been_observed_about_is_left_alone() {
        assert!(!quiet(&on_request(InstanceState::Running), Some(TIMEOUT_MS), None));
        assert!(!quiet(&on_request(InstanceState::Running), None, Some(QUIET_SINCE_MS)));
    }
}
