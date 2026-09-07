//! How long a start waits after the one before it failed.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackoffPolicy {
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_factor: f64,
}

impl From<&protocol::RestartPolicy> for BackoffPolicy {
    fn from(policy: &protocol::RestartPolicy) -> Self {
        Self {
            initial_backoff_ms: policy.initial_backoff_ms,
            max_backoff_ms: policy.max_backoff_ms,
            backoff_factor: policy.backoff_factor,
        }
    }
}

pub fn backoff_delay_ms(attempt: u32, policy: &BackoffPolicy) -> u64 {
    if attempt == 0 {
        return 0;
    }
    let grown = policy.initial_backoff_ms as f64 * policy.backoff_factor.powi(attempt as i32 - 1);
    (grown.round() as u64).min(policy.max_backoff_ms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptWindow {
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at_ms: Option<i64>,
}

/// A budget nothing has spent: what an instance is born with, and what a deliberate stop gives
/// back.
pub const NO_START_ATTEMPTS: AttemptWindow = AttemptWindow {
    attempts: 0,
    last_attempt_at_ms: None,
};

/// Staying up longer than `reset_after_ms` restarts the budget, so a monthly failure never
/// exhausts it.
pub fn next_attempt_window(window: &AttemptWindow, now_ms: i64, reset_after_ms: u64) -> AttemptWindow {
    let elapsed = window.last_attempt_at_ms.map_or(0, |at| now_ms - at);
    AttemptWindow {
        attempts: if elapsed >= reset_after_ms as i64 {
            1
        } else {
            window.attempts + 1
        },
        last_attempt_at_ms: Some(now_ms),
    }
}

pub fn is_ready_to_retry(window: &AttemptWindow, now_ms: i64, policy: &BackoffPolicy) -> bool {
    match window.last_attempt_at_ms {
        None => true,
        Some(at) => now_ms - at >= backoff_delay_ms(window.attempts, policy) as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::DEFAULT_RESTART_POLICY;

    fn policy() -> BackoffPolicy {
        BackoffPolicy::from(&DEFAULT_RESTART_POLICY)
    }

    #[test]
    fn the_delay_grows_by_the_factor_and_is_capped() {
        assert_eq!(backoff_delay_ms(0, &policy()), 0);
        assert_eq!(backoff_delay_ms(1, &policy()), 500);
        assert_eq!(backoff_delay_ms(2, &policy()), 1_000);
        assert_eq!(backoff_delay_ms(3, &policy()), 2_000);
        assert_eq!(
            backoff_delay_ms(100, &policy()),
            DEFAULT_RESTART_POLICY.max_backoff_ms
        );
    }

    #[test]
    fn a_factor_of_one_degenerates_to_a_constant_delay_rather_than_to_zero() {
        let flat = BackoffPolicy {
            initial_backoff_ms: 250,
            max_backoff_ms: 1_000,
            backoff_factor: 1.0,
        };
        assert_eq!(backoff_delay_ms(3, &flat), 250);
    }

    #[test]
    fn attempts_accumulate_inside_the_reset_window_and_a_long_gap_resets_the_budget() {
        let first = next_attempt_window(&NO_START_ATTEMPTS, 0, 60_000);
        assert_eq!(
            first,
            AttemptWindow {
                attempts: 1,
                last_attempt_at_ms: Some(0)
            }
        );
        assert_eq!(next_attempt_window(&first, 1_000, 60_000).attempts, 2);
        let spent = AttemptWindow {
            attempts: DEFAULT_RESTART_POLICY.max_restarts,
            last_attempt_at_ms: Some(0),
        };
        assert_eq!(next_attempt_window(&spent, 120_000, 60_000).attempts, 1);
    }

    #[test]
    fn a_retry_inside_the_backoff_is_refused_and_allowed_once_it_lapses() {
        assert!(is_ready_to_retry(&NO_START_ATTEMPTS, 0, &policy()));
        let window = AttemptWindow {
            attempts: 3,
            last_attempt_at_ms: Some(0),
        };
        let delay = backoff_delay_ms(3, &policy()) as i64;
        assert!(!is_ready_to_retry(&window, delay - 1, &policy()));
        assert!(is_ready_to_retry(&window, delay, &policy()));
    }
}
