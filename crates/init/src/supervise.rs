use guest_contract::instance_env::InstanceConfig;

pub(crate) const SHUTDOWN_GRACE_MS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    ShutdownRequested,
    RestartBudgetExhausted,
    SpawnFailed,
}

pub(crate) fn backoff_ms(config: &InstanceConfig, restart_count: u32) -> u32 {
    let mut delay = f64::from(config.initial_backoff_ms);
    for _ in 0..restart_count {
        delay *= config.backoff_factor;
        if delay >= f64::from(config.max_backoff_ms) {
            return config.max_backoff_ms;
        }
    }
    delay.min(f64::from(config.max_backoff_ms)) as u32
}

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
