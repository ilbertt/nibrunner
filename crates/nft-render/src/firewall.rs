use protocol::{AppId, HostPort, HttpPort, Ipv4Address};

use crate::slot::{GUEST_NETWORK_CIDR, TAP_NAME_PREFIX};

pub const NFTABLES_TABLE: &str = "nibrun";

pub const NFTABLES_FAMILIES: [&str; 2] = ["ip", "ip6"];

pub const INSTANCE_METADATA_ADDRESS_V4: &str = "169.254.169.254";
pub const INSTANCE_METADATA_ADDRESS_V6: &str = "fd00:ec2::254";

const OUTPUT_NAT_PRIORITY: i32 = -100;

const TRAFFIC_CHAIN_PRIORITY: &str = "filter + 10";

const PRIVATE_DESTINATIONS_V4: [&str; 6] = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "127.0.0.0/8",
    "100.64.0.0/10",
];

const PRIVATE_DESTINATIONS_V6: [&str; 3] = ["::1/128", "fe80::/10", "fc00::/7"];

const CHAIN_INDENT: &str = "  ";
const RULE_INDENT: &str = "    ";

const DENY: &str = "reject";

pub fn app_counter_name(app_id: &AppId) -> String {
    format!("app_{app_id}")
}

pub const APP_COUNTER_PREFIX: &str = "app_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedInstance {
    pub app_id: AppId,
    pub host_port: HostPort,
    pub http_port: HttpPort,
    pub extra_public_port: Option<HostPort>,
    pub host_ipv4: Ipv4Address,
    pub guest_ipv4: Ipv4Address,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FirewallState {
    pub instances: Vec<ForwardedInstance>,
    pub control_plane_cidrs_v4: Vec<String>,
    pub control_plane_cidrs_v6: Vec<String>,
}

fn tap_match() -> String {
    format!("\"{TAP_NAME_PREFIX}*\"")
}

fn set(values: &[&str]) -> String {
    format!("{{ {} }}", values.join(", "))
}

fn counter_objects(state: &FirewallState) -> Vec<String> {
    state
        .instances
        .iter()
        .flat_map(|instance| {
            [
                format!("{CHAIN_INDENT}counter {} {{", app_counter_name(&instance.app_id)),
                format!("{CHAIN_INDENT}}}"),
                String::new(),
            ]
        })
        .collect()
}

fn chain(header: &str, rules: &[String]) -> Vec<String> {
    let mut lines = vec![format!("{CHAIN_INDENT}chain {header}")];
    lines.extend(rules.iter().map(|rule| format!("{RULE_INDENT}{rule}")));
    lines.push(format!("{CHAIN_INDENT}}}"));
    lines
}

fn table(family: &str, body: Vec<String>) -> Vec<String> {
    let mut lines = vec![
        format!("table {family} {NFTABLES_TABLE}"),
        format!("delete table {family} {NFTABLES_TABLE}"),
        String::new(),
        format!("table {family} {NFTABLES_TABLE} {{"),
    ];
    lines.extend(body);
    lines.push("}".to_string());
    lines
}

pub fn render_ruleset(state: &FirewallState) -> String {
    let mut lines = Vec::new();
    let mut v4_body = counter_objects(state);
    v4_body.extend(forward_chain_v4(state));
    v4_body.push(String::new());
    v4_body.extend(input_chain_v4());
    v4_body.push(String::new());
    v4_body.extend(nat_chains_v4(state));
    v4_body.push(String::new());
    v4_body.extend(traffic_chains_v4(state));
    lines.extend(table("ip", v4_body));
    lines.push(String::new());
    let mut v6_body = forward_chain_v6(state);
    v6_body.push(String::new());
    v6_body.extend(input_chain_v6());
    lines.extend(table("ip6", v6_body));
    format!("{}\n", lines.join("\n"))
}

fn traffic_chains_v4(state: &FirewallState) -> Vec<String> {
    if state.instances.is_empty() {
        return Vec::new();
    }
    let tap = tap_match();
    let mut output_rules = vec!["type filter hook output priority filter; policy accept;".to_string()];
    output_rules.extend(state.instances.iter().map(|instance| {
        format!(
            "ip saddr 127.0.0.0/8 ip daddr {} tcp dport {} counter name {}",
            instance.guest_ipv4,
            instance.http_port,
            app_counter_name(&instance.app_id)
        )
    }));
    let mut forward_rules = vec![format!(
        "type filter hook forward priority {TRAFFIC_CHAIN_PRIORITY}; policy accept;"
    )];
    forward_rules.extend(state.instances.iter().map(|instance| {
        format!(
            "iifname != {tap} oifname {tap} ip daddr {} counter name {}",
            instance.guest_ipv4,
            app_counter_name(&instance.app_id)
        )
    }));
    let mut lines = chain("traffic_output {", &output_rules);
    lines.push(String::new());
    lines.extend(chain("traffic_forward {", &forward_rules));
    lines
}

fn forward_chain_v4(state: &FirewallState) -> Vec<String> {
    let tap = tap_match();
    let mut rules = vec![
        "type filter hook forward priority filter; policy accept;".to_string(),
        "ct state established,related accept".to_string(),
        format!("iifname {tap} ip daddr {INSTANCE_METADATA_ADDRESS_V4} {DENY} comment \"instance metadata endpoint\""),
        format!("iifname {tap} oifname {tap} {DENY} comment \"guest to guest\""),
        format!("iifname {tap} ip daddr {GUEST_NETWORK_CIDR} {DENY} comment \"guest to guest\""),
    ];
    rules.extend(
        state
            .control_plane_cidrs_v4
            .iter()
            .map(|cidr| format!("iifname {tap} ip daddr {cidr} {DENY} comment \"control plane\"")),
    );
    rules.push(format!(
        "iifname {tap} ip daddr {} {DENY} comment \"private destinations\"",
        set(&PRIVATE_DESTINATIONS_V4)
    ));
    chain("forward {", &rules)
}

fn input_chain_v4() -> Vec<String> {
    let tap = tap_match();
    chain(
        "input {",
        &[
            "type filter hook input priority filter; policy accept;".to_string(),
            format!("iifname {tap} ct state established,related accept"),
            format!("iifname {tap} {DENY} comment \"guest to host\""),
        ],
    )
}

fn forward_chain_v6(state: &FirewallState) -> Vec<String> {
    let tap = tap_match();
    let mut rules = vec![
        "type filter hook forward priority filter; policy accept;".to_string(),
        "ct state established,related accept".to_string(),
        format!("iifname {tap} ip6 daddr {INSTANCE_METADATA_ADDRESS_V6} {DENY} comment \"instance metadata endpoint\""),
        format!("iifname {tap} oifname {tap} {DENY} comment \"guest to guest\""),
    ];
    rules.extend(
        state
            .control_plane_cidrs_v6
            .iter()
            .map(|cidr| format!("iifname {tap} ip6 daddr {cidr} {DENY} comment \"control plane\"")),
    );
    rules.push(format!(
        "iifname {tap} ip6 daddr {} {DENY} comment \"private destinations\"",
        set(&PRIVATE_DESTINATIONS_V6)
    ));
    chain("forward {", &rules)
}

fn input_chain_v6() -> Vec<String> {
    let tap = tap_match();
    chain(
        "input {",
        &[
            "type filter hook input priority filter; policy accept;".to_string(),
            format!("iifname {tap} ct state established,related accept"),
            format!("iifname {tap} {DENY} comment \"guest to host\""),
        ],
    )
}

fn nat_chains_v4(state: &FirewallState) -> Vec<String> {
    let tap = tap_match();
    let mut prerouting = vec!["type nat hook prerouting priority dstnat; policy accept;".to_string()];
    prerouting.extend(state.instances.iter().map(|instance| {
        format!(
            "iifname != {tap} tcp dport {} dnat to {}:{}",
            instance.host_port, instance.guest_ipv4, instance.http_port
        )
    }));
    for instance in &state.instances {
        let Some(port) = instance.extra_public_port else {
            continue;
        };
        for protocol in ["tcp", "udp"] {
            prerouting.push(format!(
                "iifname != {tap} {protocol} dport {port} dnat to {}:{port}",
                instance.guest_ipv4
            ));
        }
    }

    let mut output = vec![format!(
        "type nat hook output priority {OUTPUT_NAT_PRIORITY}; policy accept;"
    )];
    output.extend(state.instances.iter().map(|instance| {
        format!(
            "ip daddr 127.0.0.1 tcp dport {} dnat to {}:{}",
            instance.host_port, instance.guest_ipv4, instance.http_port
        )
    }));

    let mut postrouting = vec!["type nat hook postrouting priority srcnat; policy accept;".to_string()];
    postrouting.extend(state.instances.iter().map(|instance| {
        format!(
            "oifname {tap} ip saddr 127.0.0.0/8 ip daddr {} snat to {}",
            instance.guest_ipv4, instance.host_ipv4
        )
    }));
    postrouting.push(format!(
        "ip saddr {GUEST_NETWORK_CIDR} oifname != {tap} masquerade"
    ));

    let mut lines = chain("prerouting {", &prerouting);
    lines.push(String::new());
    lines.extend(chain("output {", &output));
    lines.push(String::new());
    lines.extend(chain("postrouting {", &postrouting));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> ForwardedInstance {
        ForwardedInstance {
            app_id: AppId::parse("0198f3aa-1c2d-7e4b-9f11-a0b1c2d3e4f5").unwrap(),
            host_port: HostPort::new(21_000).unwrap(),
            http_port: HttpPort::new(3000).unwrap(),
            extra_public_port: None,
            host_ipv4: Ipv4Address::parse("10.201.0.1").unwrap(),
            guest_ipv4: Ipv4Address::parse("10.201.0.2").unwrap(),
        }
    }

    fn asked_for_a_port() -> ForwardedInstance {
        ForwardedInstance {
            extra_public_port: HostPort::new(22_000).ok(),
            ..instance()
        }
    }

    fn state(instances: Vec<ForwardedInstance>, v4: &[&str], v6: &[&str]) -> FirewallState {
        FirewallState {
            instances,
            control_plane_cidrs_v4: v4.iter().map(|s| s.to_string()).collect(),
            control_plane_cidrs_v6: v6.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn refusals(ruleset: &str) -> Vec<String> {
        ruleset
            .lines()
            .map(str::trim)
            .filter(|line| line.contains(" reject"))
            .map(str::to_string)
            .collect()
    }

    fn v6_half(ruleset: &str) -> String {
        ruleset
            .split(&format!("table ip6 {NFTABLES_TABLE} {{"))
            .nth(1)
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn the_isolation_rules_are_never_optional() {
        let empty = render_ruleset(&state(vec![], &[], &[]));
        assert!(refusals(&empty)
            .iter()
            .any(|l| l.contains(INSTANCE_METADATA_ADDRESS_V4)));
        assert!(refusals(&empty).iter().any(|l| l.contains("guest to host")));
        let joined = refusals(&empty).join("\n");
        for cidr in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16"] {
            assert!(joined.contains(cidr));
        }
        let with_instance = render_ruleset(&state(vec![instance()], &[], &[]));
        assert!(refusals(&with_instance)
            .iter()
            .any(|l| l.contains("guest to guest")));
        assert!(refusals(&with_instance)
            .iter()
            .any(|l| l.contains("10.201.0.0/16")));
        let control = render_ruleset(&state(vec![], &["203.0.113.10/32"], &[]));
        assert!(refusals(&control).iter().any(|l| l.contains("203.0.113.10/32")));
    }

    #[test]
    fn a_guest_cannot_reach_the_control_plane_before_it_is_allowed_out() {
        let vpc = "10.43.0.0/16";
        let ruleset = render_ruleset(&state(vec![], &[vpc], &[]));
        let lines: Vec<&str> = ruleset.lines().collect();
        let denied = lines
            .iter()
            .position(|l| l.contains(vpc) && l.contains("reject"))
            .unwrap();
        let allowed_out = lines.iter().position(|l| l.contains("masquerade")).unwrap();
        assert!(allowed_out > denied);
    }

    #[test]
    fn nothing_is_denied_silently_in_either_family() {
        let ruleset = render_ruleset(&state(
            vec![instance()],
            &["10.43.0.0/16"],
            &["2600:1f18:abcd::/56"],
        ));
        assert!(!ruleset.contains("drop"));
    }

    #[test]
    fn the_same_isolation_holds_over_ipv6() {
        let ruleset = render_ruleset(&state(vec![], &[], &[]));
        let v6 = v6_half(&ruleset);
        assert!(v6.contains("guest to host"));
        assert!(v6.contains("fe80::/10"));
        assert!(v6.contains("guest to guest"));
        assert!(v6.contains(INSTANCE_METADATA_ADDRESS_V6));
        assert!(v6.contains("::1/128"));
        assert!(v6.contains("fc00::/7"));
        assert!(!v6.contains("::/0"));
        assert!(ruleset.contains(&format!(
            "table ip6 {NFTABLES_TABLE}\ndelete table ip6 {NFTABLES_TABLE}\n"
        )));
    }

    #[test]
    fn the_vpc_v6_range_is_denied_by_name_before_anything_lets_a_guest_out() {
        let vpc = "2600:1f18:abcd::/56";
        let v6 = v6_half(&render_ruleset(&state(vec![], &[], &[vpc])));
        assert!(v6.contains(&format!("ip6 daddr {vpc} reject")));
        let lines: Vec<&str> = v6.lines().map(str::trim).collect();
        let denied = lines.iter().position(|l| l.contains(vpc)).unwrap();
        let blanket = lines
            .iter()
            .position(|l| l.contains("private destinations"))
            .unwrap();
        assert!(blanket > denied);
        assert!(!["::1", "fe80:", "fc", "fd"]
            .iter()
            .any(|range| vpc.starts_with(range)));
    }

    #[test]
    fn forwarding_reaches_the_http_port_and_only_when_something_runs() {
        let ruleset = render_ruleset(&state(vec![instance()], &[], &[]));
        assert!(ruleset
            .lines()
            .any(|l| l.contains("tcp dport 21000") && l.contains("dnat to 10.201.0.2:3000")));
        assert!(!render_ruleset(&state(vec![], &[], &[])).contains("dnat to"));
        assert!(ruleset.contains("ip saddr 10.201.0.0/16 oifname != \"nbr*\" masquerade"));
        let output = ruleset.lines().find(|l| l.contains("hook output")).unwrap();
        assert!(output.contains("priority -100"));
        assert!(!ruleset.contains("hook output priority dstnat"));
        assert!(ruleset.contains("ip saddr 127.0.0.0/8 ip daddr 10.201.0.2 snat to 10.201.0.1"));
    }

    #[test]
    fn the_port_an_app_asked_for_arrives_as_the_port_it_was_sent_to() {
        let ruleset = render_ruleset(&state(vec![asked_for_a_port()], &[], &[]));
        assert!(ruleset
            .lines()
            .any(|l| l.contains("tcp dport 22000") && l.contains("dnat to 10.201.0.2:22000")));
        assert!(ruleset
            .lines()
            .any(|l| l.contains("udp dport 22000") && l.contains("dnat to 10.201.0.2:22000")));
        let rules: Vec<&str> = ruleset.lines().filter(|l| l.contains("dport 22000")).collect();
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|l| l.trim().starts_with("iifname != \"nbr*\"")));
        let without = render_ruleset(&state(vec![instance()], &[], &[]));
        assert!(!without.contains("22000"));
        assert!(!without.contains("udp dport"));
    }

    #[test]
    fn the_ruleset_is_a_function_of_state_not_a_history_of_edits() {
        let input = state(vec![instance()], &["203.0.113.0/24"], &[]);
        let ruleset = render_ruleset(&input);
        assert!(ruleset.starts_with(&format!(
            "table ip {NFTABLES_TABLE}\ndelete table ip {NFTABLES_TABLE}\n"
        )));
        assert_eq!(ruleset, render_ruleset(&input));
    }

    #[test]
    fn an_app_is_counted_wherever_it_is_reached_and_nowhere_it_is_forwarded() {
        let ruleset = render_ruleset(&state(vec![instance()], &[], &[]));
        let name = app_counter_name(&instance().app_id);
        assert!(ruleset.contains(&format!("counter {name} {{")));
        assert!(!ruleset.contains(&format!("counter \"{name}\"")));
        let both = render_ruleset(&state(vec![instance(), asked_for_a_port()], &[], &[]));
        for rule in both.lines().filter(|l| l.contains("dnat to")) {
            assert!(!rule.contains("counter"));
        }
        let loopback: Vec<&str> = ruleset
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("ip saddr 127.0.0.0/8") && l.contains("counter name"))
            .collect();
        assert_eq!(loopback.len(), 1);
        assert!(loopback[0].contains("ip daddr 10.201.0.2"));
        assert!(loopback[0].contains(&name));
        let forwarded: Vec<&str> = ruleset
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("oifname \"nbr*\" ip daddr") && l.contains("counter name"))
            .collect();
        assert_eq!(forwarded.len(), 1);
        let counted: Vec<&str> = ruleset.lines().filter(|l| l.contains("counter name")).collect();
        assert_eq!(counted.len(), 2);
        let with_port = render_ruleset(&state(vec![asked_for_a_port()], &[], &[]));
        assert_eq!(
            with_port.lines().filter(|l| l.contains("counter name")).count(),
            2
        );
        assert!(ruleset.contains("type filter hook forward priority filter + 10;"));
        let empty = render_ruleset(&state(vec![], &[], &[]));
        assert!(!empty.contains("counter "));
        assert!(!empty.contains("traffic_"));
    }
}
