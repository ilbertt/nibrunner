//! `/instance.env`, the file the config drive carries. Line-oriented `KEY=VALUE`, so the guest's
//! init needs no parser. Two namespaces and nothing else: `NIBRUN_<KEY>` for what the runtime
//! itself needs, `ENV_<NAME>` for one of the tenant's own variables. The prefixes exist so the two
//! can never collide. `apps/runtime/src/config.h` is the format contract.

use protocol::{AppHostname, AppHostnameKind, HostPort, Hostname, HttpPort, Ipv4Address, RestartPolicy, TenantArguments, TenantEnvironment};

pub const INSTANCE_ENV_FILENAME: &str = "instance.env";
pub const INSTANCE_CONFIG_IMAGE: &str = "config.squashfs";

/// The two namespaces the runtime parses, so a tenant variable called NIBRUN_HTTP_PORT stays the
/// tenant's.
const RUNTIME_PREFIX: &str = "NIBRUN_";
const TENANT_PREFIX: &str = "ENV_";

/// Public rather than the VPC's: the guest network is cut off from every private destination, and
/// a resolver inside one would mean opening that back up for whatever else answers on the address.
const DNS_SERVERS: [&str; 2] = ["1.1.1.1", "1.0.0.1"];

/// Where a tenant's own port is reached, for an app that asked for one. The two together, because
/// half of what a binary has to announce is not an announcement, and neither half is something a
/// guest can find out for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicAddress {
    pub ipv4: Ipv4Address,
    pub port: HostPort,
}

#[derive(Debug, Clone)]
pub struct InstanceEnvContent<'a> {
    pub http_port: HttpPort,
    pub public_address: Option<PublicAddress>,
    pub hostnames: &'a [AppHostname],
    pub args: &'a TenantArguments,
    pub environment: &'a TenantEnvironment,
    pub restart_policy: &'a RestartPolicy,
}

/// Names the variable and never its value, which is the tenant's secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{variable_name} has no representation on the config drive")]
pub struct UnrepresentableEnvironment {
    pub variable_name: String,
}

fn has_forbidden_character(value: &str) -> bool {
    value.contains(['\n', '\r', '\0'])
}

/// The name nibrun issued the app, never one its owner brought: it is the hostname the app is
/// always reachable at, and the only one that cannot be taken away underneath a running binary.
fn platform_hostname(hostnames: &[AppHostname]) -> Option<&Hostname> {
    hostnames.iter().find(|each| each.kind == AppHostnameKind::Platform).map(|each| &each.hostname)
}

/// `backoffFactor` is written the way JavaScript prints a number: `2` rather than `2.0`, which is
/// what the guest's `strtod` reads and what the reference implementation's test asserts.
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// A value containing a newline has no representation in a format with no quoting, so it fails
/// the instance rather than truncating somebody's configuration into the next line.
pub fn render_instance_env(content: &InstanceEnvContent<'_>) -> Result<String, UnrepresentableEnvironment> {
    let mut lines = vec![format!("{RUNTIME_PREFIX}HTTP_PORT={}", content.http_port)];
    if let Some(hostname) = platform_hostname(content.hostnames) {
        lines.push(format!("{RUNTIME_PREFIX}HOSTNAME={hostname}"));
    }
    if let Some(public) = &content.public_address {
        lines.push(format!("{RUNTIME_PREFIX}PUBLIC_IPV4={}", public.ipv4));
        lines.push(format!("{RUNTIME_PREFIX}EXTRA_PUBLIC_PORT={}", public.port));
    }
    let policy = content.restart_policy;
    lines.push(format!("{RUNTIME_PREFIX}MAX_RESTARTS={}", policy.max_restarts));
    lines.push(format!("{RUNTIME_PREFIX}INITIAL_BACKOFF_MS={}", policy.initial_backoff_ms));
    lines.push(format!("{RUNTIME_PREFIX}MAX_BACKOFF_MS={}", policy.max_backoff_ms));
    lines.push(format!("{RUNTIME_PREFIX}BACKOFF_FACTOR={}", js_number(policy.backoff_factor)));
    lines.push(format!("{RUNTIME_PREFIX}RESET_AFTER_MS={}", policy.reset_after_ms));
    lines.push(format!("{RUNTIME_PREFIX}DNS={}", DNS_SERVERS.join(",")));
    // Numbered rather than delimited: a format with no quoting cannot carry a separator an
    // argument might itself contain, and the guest refuses a gap rather than shifting the rest down.
    for (index, argument) in content.args.iter().enumerate() {
        if has_forbidden_character(argument) {
            return Err(UnrepresentableEnvironment { variable_name: format!("{RUNTIME_PREFIX}ARG_{index}") });
        }
        lines.push(format!("{RUNTIME_PREFIX}ARG_{index}={argument}"));
    }
    for (name, value) in content.environment.iter() {
        if has_forbidden_character(value.expose()) {
            return Err(UnrepresentableEnvironment { variable_name: name.to_string() });
        }
        lines.push(format!("{TENANT_PREFIX}{name}={}", value.expose()));
    }
    Ok(format!("{}\n", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{TenantValue, DEFAULT_HTTP_PORT, DEFAULT_RESTART_POLICY};

    const PLATFORM_HOSTNAME: &str = "my-app.nibrun.app";

    fn hostname(name: &str, kind: AppHostnameKind) -> AppHostname {
        AppHostname { hostname: Hostname::parse(name).unwrap(), kind }
    }

    fn environment(values: &[(&str, &str)]) -> TenantEnvironment {
        values.iter().map(|(name, value)| (name.to_string(), TenantValue::parse(*value).unwrap())).collect()
    }

    struct Overrides {
        http_port: HttpPort,
        public_address: Option<PublicAddress>,
        hostnames: Vec<AppHostname>,
        args: Vec<String>,
        environment: TenantEnvironment,
    }

    impl Default for Overrides {
        fn default() -> Self {
            Self {
                http_port: DEFAULT_HTTP_PORT,
                public_address: None,
                hostnames: vec![hostname(PLATFORM_HOSTNAME, AppHostnameKind::Platform)],
                args: vec![],
                environment: TenantEnvironment::default(),
            }
        }
    }

    fn attempt(overrides: Overrides) -> Result<String, UnrepresentableEnvironment> {
        let args = TenantArguments::try_from(overrides.args).unwrap();
        render_instance_env(&InstanceEnvContent {
            http_port: overrides.http_port,
            public_address: overrides.public_address,
            hostnames: &overrides.hostnames,
            args: &args,
            environment: &overrides.environment,
            restart_policy: &DEFAULT_RESTART_POLICY,
        })
    }

    fn render(overrides: Overrides) -> String {
        attempt(overrides).unwrap()
    }

    #[test]
    fn every_key_it_writes_is_one_the_runtime_knows() {
        let rendered = render(Overrides::default());
        let lines: Vec<&str> = rendered.lines().filter(|line| !line.is_empty()).collect();
        let expected = [
            "NIBRUN_HTTP_PORT=3000",
            "NIBRUN_HOSTNAME=my-app.nibrun.app",
            "NIBRUN_MAX_RESTARTS=5",
            "NIBRUN_INITIAL_BACKOFF_MS=500",
            "NIBRUN_MAX_BACKOFF_MS=30000",
            "NIBRUN_BACKOFF_FACTOR=2",
            "NIBRUN_RESET_AFTER_MS=60000",
            "NIBRUN_DNS=1.1.1.1,1.0.0.1",
        ];
        assert_eq!(lines, expected);
    }

    #[test]
    fn an_app_that_asked_for_a_public_port_is_told_the_address_and_the_port_together() {
        let without = render(Overrides::default());
        assert!(!without.contains("NIBRUN_PUBLIC_IPV4"));
        assert!(!without.contains("NIBRUN_EXTRA_PUBLIC_PORT"));
        let with = render(Overrides {
            public_address: Some(PublicAddress { ipv4: Ipv4Address::parse("203.0.113.7").unwrap(), port: HostPort::new(22_000).unwrap() }),
            ..Default::default()
        });
        let lines: Vec<&str> = with.lines().collect();
        assert_eq!(lines[1], "NIBRUN_HOSTNAME=my-app.nibrun.app");
        assert_eq!(lines[2], "NIBRUN_PUBLIC_IPV4=203.0.113.7");
        assert_eq!(lines[3], "NIBRUN_EXTRA_PUBLIC_PORT=22000");
    }

    #[test]
    fn tenant_variables_carry_the_tenant_prefix_in_a_stable_order() {
        let rendered = render(Overrides { environment: environment(&[("ZED", "1"), ("ALPHA", "2")]), ..Default::default() });
        assert!(rendered.contains("\nENV_ALPHA=2\nENV_ZED=1\n"));
        let shadowing = render(Overrides { environment: environment(&[("NIBRUN_HTTP_PORT", "9999")]), ..Default::default() });
        assert!(shadowing.contains("NIBRUN_HTTP_PORT=3000\n"));
        assert!(shadowing.contains("ENV_NIBRUN_HTTP_PORT=9999\n"));
        let raw = render(Overrides { environment: environment(&[("DSN", "postgres://u:p@h/db?x=1 y=2")]), ..Default::default() });
        assert!(raw.contains("ENV_DSN=postgres://u:p@h/db?x=1 y=2\n"));
        assert!(render(Overrides { environment: environment(&[("EMPTY", "")]), ..Default::default() }).contains("ENV_EMPTY=\n"));
    }

    #[test]
    fn the_hostname_written_is_the_platform_one_or_nothing() {
        let rendered = render(Overrides {
            hostnames: vec![hostname("www.example.com", AppHostnameKind::Custom), hostname(PLATFORM_HOSTNAME, AppHostnameKind::Platform)],
            ..Default::default()
        });
        assert!(rendered.contains("NIBRUN_HOSTNAME=my-app.nibrun.app\n"));
        assert!(!rendered.contains("www.example.com"));
        assert!(!render(Overrides { hostnames: vec![], ..Default::default() }).contains("NIBRUN_HOSTNAME"));
        assert!(render(Overrides { http_port: HttpPort::new(8080).unwrap(), ..Default::default() }).contains("NIBRUN_HTTP_PORT=8080\n"));
    }

    #[test]
    fn what_has_no_representation_fails_the_instance_without_leaking_the_value() {
        for character in ["\n", "\r", "\0"] {
            let refused = attempt(Overrides { environment: environment(&[("BAD", &format!("a{character}INJECTED=1"))]), ..Default::default() }).unwrap_err();
            assert_eq!(refused.variable_name, "BAD");
        }
        let refused = attempt(Overrides { environment: environment(&[("API_KEY", "secret-value\nmore")]), ..Default::default() }).unwrap_err();
        assert_eq!(refused.variable_name, "API_KEY");
        assert!(!format!("{refused:?}").contains("secret-value"));
    }

    #[test]
    fn arguments_reach_the_guest_as_the_user_wrote_them() {
        let rendered = render(Overrides { args: vec!["serve".into(), "--http=0.0.0.0:8090".into()], ..Default::default() });
        assert!(rendered.contains("NIBRUN_ARG_0=serve"));
        assert!(rendered.contains("NIBRUN_ARG_1=--http=0.0.0.0:8090"));
        assert!(!render(Overrides::default()).contains("NIBRUN_ARG_"));
        let refused = attempt(Overrides { args: vec!["--flag=one\ntwo".into()], ..Default::default() }).unwrap_err();
        assert_eq!(refused.variable_name, "NIBRUN_ARG_0");
    }
}
