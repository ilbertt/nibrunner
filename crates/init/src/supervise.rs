//! Running the tenant until it is asked to stop or has run out of restarts.
//!
//! Ported from `apps/runtime/src/supervise.c`.

use guest_contract::instance_env::InstanceConfig;

/// How long the tenant gets between SIGTERM and SIGKILL. The host's own wait for the microVM to
/// exit has to be longer than this, or a tenant that takes its time shutting down looks the same
/// as one that hung.
pub(crate) const SHUTDOWN_GRACE_MS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    ShutdownRequested,
    /// Deliberately not another restart: the guest ends, the host reports the instance failed, and
    /// what happens next is the reconciler's call. A guest that retried for ever on its own would
    /// hide a broken deploy.
    RestartBudgetExhausted,
    SpawnFailed,
}

/// Exponential from the initial delay, capped, and computed rather than accumulated so a restart
/// count that came back from anywhere gives the same answer.
///
/// The first restart waits the initial backoff rather than nothing: a tenant that exits instantly
/// on a bad config would otherwise spend its whole budget inside one scheduler tick, and the point
/// of a budget is to leave time for somebody to look.
pub(crate) fn backoff_ms(config: &InstanceConfig, restart_count: u32) -> u32 {
    let mut delay = f64::from(config.initial_backoff_ms);
    for _ in 0..restart_count {
        delay *= config.backoff_factor;
        if delay >= f64::from(config.max_backoff_ms) {
            return config.max_backoff_ms;
        }
    }
    // Truncating rather than rounding: the cap above is the only bound that matters, and a
    // fractional millisecond is not a thing anybody waits for.
    delay.min(f64::from(config.max_backoff_ms)) as u32
}

/// Whether a tenant that has been up this long has earned its budget back.
///
/// Measured from the last start rather than from the first: an app that has been serving for an
/// hour and then crashes is not the same app as one that has crashed five times in a minute, and
/// treating them alike spends the budget of the first on the history of the second.
pub(crate) fn budget_resets(config: &InstanceConfig, uptime_ms: u64) -> bool {
    uptime_ms >= u64::from(config.reset_after_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(edit: impl FnOnce(&mut InstanceConfig)) -> InstanceConfig {
        let mut value = InstanceConfig {
            http_port: 3000,
            hostname: None,
            public_ipv4: None,
            extra_public_port: None,
            max_restarts: 5,
            initial_backoff_ms: 500,
            max_backoff_ms: 30_000,
            backoff_factor: 2.0,
            reset_after_ms: 60_000,
            nameservers: vec![],
            arguments: vec![],
            environment: vec![],
        };
        edit(&mut value);
        value
    }

    #[test]
    fn the_first_restart_waits_rather_than_going_straight_back() {
        assert_eq!(backoff_ms(&config(|_| {}), 0), 500);
    }

    #[test]
    fn each_restart_waits_longer_until_the_cap() {
        let config = config(|_| {});
        assert_eq!(backoff_ms(&config, 1), 1_000);
        assert_eq!(backoff_ms(&config, 2), 2_000);
        assert_eq!(backoff_ms(&config, 3), 4_000);
        assert_eq!(backoff_ms(&config, 20), 30_000, "capped, not overflowed");
    }

    #[test]
    fn the_delay_is_a_function_of_the_count_and_not_of_the_history() {
        let config = config(|_| {});
        for count in 0..10 {
            assert_eq!(backoff_ms(&config, count), backoff_ms(&config, count));
        }
    }

    /// A factor of 1 is a fixed delay, which is a policy somebody may want and not a bug.
    #[test]
    fn a_factor_that_does_not_grow_is_a_fixed_delay() {
        let config = config(|config| config.backoff_factor = 1.0);
        for count in 0..10 {
            assert_eq!(backoff_ms(&config, count), 500);
        }
    }

    #[test]
    fn a_tenant_that_stayed_up_earns_its_budget_back() {
        let config = config(|_| {});
        assert!(!budget_resets(&config, 59_999));
        assert!(budget_resets(&config, 60_000));
    }
}
