use protocol::{ActivationPolicy, InstanceState, SleepPolicy};

use crate::domain::report::InstanceRecord;

/// The only state with a running microVM to put down. Everything else is either on its way
/// somewhere already or has nothing to suspend.
const SLEEPABLE_STATES: [InstanceState; 1] = [InstanceState::Running];

/// Why this host is putting a microVM to sleep, in the word the log and the report use for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepReason {
    Quiet,
    LivedLongEnough,
}

impl SleepReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SleepReason::Quiet => "idle",
            SleepReason::LivedLongEnough => "max-lifetime",
        }
    }
}

/// What a pass has observed about an instance since it was last started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivitySignals {
    pub last_active_at_ms: Option<i64>,
    pub started_at_ms: Option<i64>,
}

/// Whether a policy is one this host can act on without any signal from inside the guest.
pub fn should_sleep(
    policy: &ActivationPolicy,
    record: &InstanceRecord,
    signals: &ActivitySignals,
    now_ms: i64,
) -> Option<SleepReason> {
    if !record.on_request || !record.desired_running || !SLEEPABLE_STATES.contains(&record.state) {
        return None;
    }
    match policy.sleep_when {
        SleepPolicy::Never => None,
        SleepPolicy::TrafficIdle { timeout_ms } => {
            let quiet_since = signals.last_active_at_ms?;
            (now_ms - quiet_since >= timeout_ms.get() as i64).then_some(SleepReason::Quiet)
        }
        SleepPolicy::MaxLifetime { ttl_ms } => {
            let up_since = signals.started_at_ms?;
            (now_ms - up_since >= ttl_ms.get() as i64).then_some(SleepReason::LivedLongEnough)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::instance_record;
    use protocol::{IdleTimeoutMs, MaxLifetimeMs, INSTANCE_STATES};

    const NOW_MS: i64 = 10_000_000;
    const TIMEOUT_MS: u64 = 900_000;
    const TTL_MS: u64 = 3_600_000;

    fn traffic_idle() -> ActivationPolicy {
        ActivationPolicy {
            sleep_when: SleepPolicy::TrafficIdle {
                timeout_ms: IdleTimeoutMs::try_from(TIMEOUT_MS).unwrap(),
            },
        }
    }

    fn max_lifetime() -> ActivationPolicy {
        ActivationPolicy {
            sleep_when: SleepPolicy::MaxLifetime {
                ttl_ms: MaxLifetimeMs::try_from(TTL_MS).unwrap(),
            },
        }
    }

    fn never() -> ActivationPolicy {
        ActivationPolicy {
            sleep_when: SleepPolicy::Never,
        }
    }

    fn serving(state: InstanceState) -> InstanceRecord {
        instance_record(|record| {
            record.on_request = true;
            record.desired_running = true;
            record.state = state;
        })
    }

    fn quiet_since(ms: i64) -> ActivitySignals {
        ActivitySignals {
            last_active_at_ms: Some(ms),
            started_at_ms: None,
        }
    }

    fn up_since(ms: i64) -> ActivitySignals {
        ActivitySignals {
            last_active_at_ms: None,
            started_at_ms: Some(ms),
        }
    }

    #[test]
    fn a_quiet_app_sleeps_at_its_timeout_and_not_a_moment_before() {
        let record = serving(InstanceState::Running);
        let at = quiet_since(NOW_MS - TIMEOUT_MS as i64);
        assert_eq!(
            should_sleep(&traffic_idle(), &record, &at, NOW_MS),
            Some(SleepReason::Quiet)
        );
        let a_moment_ago = quiet_since(NOW_MS - TIMEOUT_MS as i64 + 1);
        assert_eq!(
            should_sleep(&traffic_idle(), &record, &a_moment_ago, NOW_MS),
            None
        );
    }

    #[test]
    fn an_app_that_has_lived_its_ttl_sleeps_however_busy_it_still_is() {
        let record = serving(InstanceState::Running);
        let signals = ActivitySignals {
            last_active_at_ms: Some(NOW_MS),
            started_at_ms: Some(NOW_MS - TTL_MS as i64),
        };
        assert_eq!(
            should_sleep(&max_lifetime(), &record, &signals, NOW_MS),
            Some(SleepReason::LivedLongEnough)
        );
        let younger = up_since(NOW_MS - TTL_MS as i64 + 1);
        assert_eq!(should_sleep(&max_lifetime(), &record, &younger, NOW_MS), None);
    }

    #[test]
    fn a_ttl_is_measured_from_the_start_and_a_timeout_from_the_last_caller() {
        let record = serving(InstanceState::Running);
        let busy_but_old = ActivitySignals {
            last_active_at_ms: Some(NOW_MS),
            started_at_ms: Some(NOW_MS - TTL_MS as i64),
        };
        assert_eq!(
            should_sleep(&traffic_idle(), &record, &busy_but_old, NOW_MS),
            None
        );

        let quiet_but_young = ActivitySignals {
            last_active_at_ms: Some(NOW_MS - TIMEOUT_MS as i64),
            started_at_ms: Some(NOW_MS),
        };
        assert_eq!(
            should_sleep(&max_lifetime(), &record, &quiet_but_young, NOW_MS),
            None
        );
    }

    #[test]
    fn a_policy_that_never_sleeps_lets_nothing_go_however_long_it_has_been() {
        let record = serving(InstanceState::Running);
        let signals = ActivitySignals {
            last_active_at_ms: Some(0),
            started_at_ms: Some(0),
        };
        assert_eq!(should_sleep(&never(), &record, &signals, NOW_MS), None);
    }

    #[test]
    fn an_app_nothing_has_been_observed_about_is_left_alone() {
        let record = serving(InstanceState::Running);
        assert_eq!(
            should_sleep(&traffic_idle(), &record, &ActivitySignals::default(), NOW_MS),
            None
        );
        assert_eq!(
            should_sleep(&max_lifetime(), &record, &ActivitySignals::default(), NOW_MS),
            None
        );
    }

    #[test]
    fn only_a_running_on_request_app_this_host_still_wants_up_is_ever_let_go() {
        let long_ago = quiet_since(0);
        for state in INSTANCE_STATES {
            let expected = (state == InstanceState::Running).then_some(SleepReason::Quiet);
            assert_eq!(
                should_sleep(&traffic_idle(), &serving(state), &long_ago, NOW_MS),
                expected,
                "{state:?}"
            );
        }

        let always_up = instance_record(|record| {
            record.on_request = false;
            record.desired_running = true;
            record.state = InstanceState::Running;
        });
        assert_eq!(should_sleep(&traffic_idle(), &always_up, &long_ago, NOW_MS), None);

        let suspended = instance_record(|record| {
            record.on_request = true;
            record.desired_running = false;
            record.state = InstanceState::Running;
        });
        assert_eq!(should_sleep(&traffic_idle(), &suspended, &long_ago, NOW_MS), None);
    }

    #[test]
    fn what_each_reason_is_called_is_what_the_report_reads() {
        assert_eq!(SleepReason::Quiet.as_str(), "idle");
        assert_eq!(SleepReason::LivedLongEnough.as_str(), "max-lifetime");
    }
}
