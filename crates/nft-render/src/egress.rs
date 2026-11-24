use std::collections::{BTreeMap, BTreeSet};

use crate::slot::TAP_NAME_PREFIX;

use protocol::AppId;
use serde_json::Value;

pub const EGRESS_TABLE: &str = "inet nibrun_egress";
pub const DESTINATIONS_PER_FAMILY: usize = 256;
const NEW_FLOW: &str =
    "meta l4proto { tcp, udp } ct direction original ct state new ct status & confirmed == 0";

/// An unchanged lease keeps its counters and timeout sets through firewall replacement.
pub type EgressBindings = BTreeMap<AppId, Option<u32>>;

pub fn egress_transaction(previous: Option<&EgressBindings>, current: &EgressBindings) -> String {
    let mut lines = Vec::new();
    if previous.is_none() {
        lines.push(format!(
            "add table {EGRESS_TABLE}\ndelete table {EGRESS_TABLE}\nadd table {EGRESS_TABLE}"
        ));
        for (map, priority) in [("before", -1), ("after", 1)] {
            lines.push(format!(
                "add map {EGRESS_TABLE} {map} {{ type ifname : verdict; }}"
            ));
            for hook in ["input", "forward"] {
                let chain = format!("{hook}_{map}");
                lines.push(format!("add chain {EGRESS_TABLE} {chain} {{ type filter hook {hook} priority {priority}; policy accept; }}"));
                lines.push(format!(
                    "add rule {EGRESS_TABLE} {chain} iifname \"{TAP_NAME_PREFIX}*\" {NEW_FLOW} iifname vmap @{map}"
                ));
            }
        }
    }
    for (app, slot) in previous.into_iter().flatten() {
        if current.get(app) == Some(slot) {
            continue;
        }
        if let Some(slot) = slot {
            for map in ["before", "after"] {
                lines.push(format!(
                    "delete element {EGRESS_TABLE} {map} {{ \"{TAP_NAME_PREFIX}{slot}\" }}"
                ));
            }
        }
        if current.contains_key(app) {
            continue;
        }
        for map in ["before", "after"] {
            lines.push(format!(
                "flush chain {EGRESS_TABLE} {map}_{app}\ndelete chain {EGRESS_TABLE} {map}_{app}"
            ));
            lines.push(format!("delete counter {EGRESS_TABLE} {map}_{app}"));
        }
        lines.push(format!("delete counter {EGRESS_TABLE} missed_{app}"));
        for family in ["ip", "ip6"] {
            lines.push(format!("delete set {EGRESS_TABLE} {family}_{app}"));
        }
    }
    for (app, slot) in current {
        if previous.and_then(|old| old.get(app)) == Some(slot) {
            continue;
        }
        if !previous.is_some_and(|old| old.contains_key(app)) {
            for map in ["before", "after"] {
                lines.push(format!(
                    "add chain {EGRESS_TABLE} {map}_{app}\nadd counter {EGRESS_TABLE} {map}_{app}"
                ));
                lines.push(format!(
                    "add rule {EGRESS_TABLE} {map}_{app} counter name {map}_{app}"
                ));
            }
            lines.push(format!("add counter {EGRESS_TABLE} missed_{app}"));
            for (family, ty, nfproto) in [("ip", "ipv4_addr", "ipv4"), ("ip6", "ipv6_addr", "ipv6")] {
                lines.push(format!("add set {EGRESS_TABLE} {family}_{app} {{ type {ty}; flags dynamic,timeout; timeout 5m; size {DESTINATIONS_PER_FAMILY}; }}"));
                lines.push(format!("add rule {EGRESS_TABLE} before_{app} meta nfproto {nfproto} update @{family}_{app} {{ ct original {family} daddr }}"));
                lines.push(format!("add rule {EGRESS_TABLE} before_{app} meta nfproto {nfproto} ct original {family} daddr != @{family}_{app} counter name missed_{app}"));
            }
        }
        if let Some(slot) = slot {
            for map in ["before", "after"] {
                lines.push(format!(
                    "add element {EGRESS_TABLE} {map} {{ \"{TAP_NAME_PREFIX}{slot}\" : jump {map}_{app} }}"
                ));
            }
        }
    }
    lines.join("\n") + "\n"
}

pub fn egress_listing(apps: &BTreeSet<AppId>) -> String {
    let mut batch = String::new();
    for app in apps {
        for (kind, prefix) in [
            ("counter", "before"),
            ("counter", "after"),
            ("counter", "missed"),
            ("set", "ip"),
            ("set", "ip6"),
        ] {
            batch.push_str(&format!("list {kind} {EGRESS_TABLE} {prefix}_{app}\n"));
        }
    }
    // nft 1.1 uses the final list command to narrow its cache. A named set here hides the
    // other sets in the same batch. Table headers carry no elements.
    batch.push_str("list tables inet\n");
    batch
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub struct EgressCounts {
    pub attempts: u64,
    pub passed: u64,
    pub destinations: usize,
    pub unrecorded: u64,
}

pub fn parse_egress(json: &str, apps: &BTreeSet<AppId>) -> Option<BTreeMap<AppId, EgressCounts>> {
    #[derive(Default)]
    struct Reading {
        before: Option<u64>,
        after: Option<u64>,
        missed: Option<u64>,
        ip: Option<usize>,
        ip6: Option<usize>,
    }
    let mut readings: BTreeMap<_, Reading> = BTreeMap::new();
    for document in serde_json::Deserializer::from_str(json).into_iter::<Value>() {
        let document = document.ok()?;
        for entry in document.get("nftables")?.as_array()? {
            let (kind, object) = match (entry.get("counter"), entry.get("set")) {
                (Some(counter), _) => ("counter", counter),
                (_, Some(set)) => ("set", set),
                _ => continue,
            };
            let (prefix, id) = object.get("name")?.as_str()?.split_once('_')?;
            let app = AppId::parse(id).ok()?;
            if !apps.contains(&app) {
                return None;
            }
            let reading = readings.entry(app).or_default();
            match (kind, prefix) {
                ("counter", "before") => reading.before = Some(object.get("packets")?.as_u64()?),
                ("counter", "after") => reading.after = Some(object.get("packets")?.as_u64()?),
                ("counter", "missed") => reading.missed = Some(object.get("packets")?.as_u64()?),
                ("set", family) => {
                    let size = match object.get("elem") {
                        Some(elements) => elements.as_array()?.len(),
                        None => 0,
                    };
                    match family {
                        "ip" => reading.ip = Some(size),
                        "ip6" => reading.ip6 = Some(size),
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
    }
    if readings.len() != apps.len() {
        return None;
    }
    readings
        .into_iter()
        .map(|(app, row)| {
            Some((
                app,
                EgressCounts {
                    attempts: row.before?,
                    passed: row.after?,
                    unrecorded: row.missed?,
                    destinations: row.ip? + row.ip6?,
                },
            ))
        })
        .collect()
}
