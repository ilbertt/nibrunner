use guest_contract::instance_env::InstanceConfig;
use guest_contract::paths;
use protocol::ArtifactKind;

pub(crate) const SHUTDOWN_GRACE_MS: u32 = 10_000;

/// What a running system is told when it is time to stop. A binary is sent SIGTERM; a system's
/// own init is asked to power off the way a container manager asks, since SIGTERM to a PID 1
/// systemd is a request to re-execute itself and not to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Halt {
    Terminate,
    PowerOff,
}

pub(crate) fn halt_for(kind: ArtifactKind) -> Halt {
    match kind {
        ArtifactKind::Executable => Halt::Terminate,
        ArtifactKind::Rootfs => Halt::PowerOff,
    }
}

/// The environment a system's init starts under. Not the data directory: inside the system there
/// is no `/app/data`, the volume is the writable half of `/`. `container` is what systemd reads
/// to learn it is not alone on the machine.
pub(crate) fn system_environment(config: &InstanceConfig) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = config
        .tenant_environment()
        .into_iter()
        .filter(|(name, _)| name != "NIBRUN_DATA_DIR")
        .collect();
    environment.push(("container".to_string(), "nibrunner".to_string()));
    environment
}

/// Where entering a system can fail before its init runs, as the exit code the child leaves
/// behind. The child runs on a borrowed stack in a copy of this process with no thread but
/// itself, so it cannot say what went wrong; the code is the whole of what it can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub(crate) enum Stumbled {
    Pipes = 120,
    Propagation = 121,
    Dev = 122,
    Pivot = 123,
    Proc = 124,
    Sys = 125,
    Scratch = 126,
    Exec = 127,
}

pub(crate) fn stumbled_on(status: i32) -> Option<&'static str> {
    Some(match status {
        120 => "its output could not be wired to this runtime",
        121 => "its mounts could not be made private",
        122 => "/dev could not be brought into it",
        123 => "its root could not be pivoted to",
        124 => "/proc could not be mounted in it",
        125 => "/sys could not be mounted in it",
        126 => "/run or /tmp could not be mounted in it",
        127 => "its init could not be executed",
        _ => return None,
    })
}

pub(crate) fn system_argv(config: &InstanceConfig) -> Vec<String> {
    std::iter::once(paths::ROOTFS_INIT.to_string())
        .chain(config.arguments.iter().cloned())
        .collect()
}

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
            artifact_kind: protocol::ArtifactKind::Executable,
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

    #[test]
    fn every_way_a_system_boot_can_stumble_is_named_and_an_ordinary_exit_is_not() {
        for stumbled in [
            Stumbled::Pipes,
            Stumbled::Propagation,
            Stumbled::Dev,
            Stumbled::Pivot,
            Stumbled::Proc,
            Stumbled::Sys,
            Stumbled::Scratch,
            Stumbled::Exec,
        ] {
            assert!(stumbled_on(stumbled as i32).is_some(), "{stumbled:?}");
        }
        assert_eq!(stumbled_on(0), None);
        assert_eq!(stumbled_on(1), None);
        assert_eq!(stumbled_on(143), None, "a signalled exit is the system's own");
    }

    #[test]
    fn a_binary_is_terminated_and_a_system_is_asked_to_power_off() {
        assert_eq!(halt_for(ArtifactKind::Executable), Halt::Terminate);
        assert_eq!(halt_for(ArtifactKind::Rootfs), Halt::PowerOff);
    }

    #[test]
    fn a_system_init_gets_the_document_arguments_after_its_own_name() {
        assert_eq!(system_argv(&config(|_| {})), vec!["/sbin/init"]);
        let with = config(|config| config.arguments = vec!["--log-level=debug".into()]);
        assert_eq!(system_argv(&with), vec!["/sbin/init", "--log-level=debug"]);
    }

    #[test]
    fn a_system_is_told_it_is_in_a_container_and_not_where_the_data_would_have_been() {
        let config = config(|config| {
            config.hostname = Some("box.example".into());
            config.environment = vec![("TOKEN".into(), "hunter2".into())];
        });
        let environment = system_environment(&config);
        let value = |name: &str| {
            environment
                .iter()
                .find(|(each, _)| each == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("container"), Some("nibrunner"));
        assert_eq!(value("NIBRUN_DATA_DIR"), None);
        assert_eq!(value("NIBRUN_HOSTNAME"), Some("box.example"));
        assert_eq!(value("TOKEN"), Some("hunter2"));
        assert_eq!(value("PORT"), Some("3000"));
    }
}
