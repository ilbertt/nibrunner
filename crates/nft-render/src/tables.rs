use std::collections::BTreeMap;

use crate::firewall::{NFTABLES_FAMILIES, NFTABLES_TABLE};

/// Which of this daemon's tables the kernel is holding, as one comparable value. The kernel
/// allocates a handle when a table is created, so a ruleset something else flushed and rebuilt
/// carries different ones and a ruleset that is simply gone carries none, which is what tells a
/// kernel still holding what was written from one that would merely be sent the same text again.
pub type KernelTables = String;

/// `nft -j list tables` answers with one object per table under a top-level `nftables` array,
/// mixed in with a `metainfo` entry. An entry that cannot be read counts as a table that is not
/// there, so the ruleset is written again rather than assumed to be in place.
pub fn parse_kernel_tables(json: &str) -> KernelTables {
    let mut handles: BTreeMap<String, u64> = BTreeMap::new();
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
        return String::new();
    };
    let Some(entries) = parsed.get("nftables").and_then(|v| v.as_array()) else {
        return String::new();
    };
    for entry in entries {
        let Some(table) = entry.get("table").and_then(|v| v.as_object()) else {
            continue;
        };
        let (Some(family), Some(name), Some(handle)) = (
            table.get("family").and_then(|v| v.as_str()),
            table.get("name").and_then(|v| v.as_str()),
            table.get("handle").and_then(|v| v.as_u64()),
        ) else {
            continue;
        };
        if name == NFTABLES_TABLE {
            handles.insert(family.to_string(), handle);
        }
    }
    // Named in the order the ruleset writes them, so the same pair of tables never renders two ways.
    NFTABLES_FAMILIES
        .iter()
        .filter_map(|family| handles.get(*family).map(|handle| format!("{family}:{handle}")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const METAINFO: &str =
        r#"{"metainfo": {"version": "1.0.4", "release_name": "Lester Gooch #3", "json_schema_version": 1}}"#;

    fn listing(tables: &[String]) -> String {
        let mut entries = vec![METAINFO.to_string()];
        entries.extend(tables.iter().cloned());
        format!(r#"{{"nftables": [{}]}}"#, entries.join(", "))
    }

    fn nibrun(family: &str, handle: u64) -> String {
        format!(r#"{{"table": {{"family": "{family}", "name": "nibrun", "handle": {handle}}}}}"#)
    }

    fn holding_both() -> String {
        listing(&[nibrun("ip", 2), nibrun("ip6", 4)])
    }

    #[test]
    fn what_the_kernel_is_holding_reads_back_as_one_comparable_value() {
        assert_eq!(
            parse_kernel_tables(&holding_both()),
            parse_kernel_tables(&holding_both())
        );
        assert_ne!(parse_kernel_tables(&holding_both()), "");
        assert_ne!(
            parse_kernel_tables(&listing(&[])),
            parse_kernel_tables(&holding_both())
        );
        assert_ne!(
            parse_kernel_tables(&listing(&[nibrun("ip", 8), nibrun("ip6", 10)])),
            parse_kernel_tables(&holding_both())
        );
        assert_ne!(
            parse_kernel_tables(&listing(&[nibrun("ip", 2)])),
            parse_kernel_tables(&holding_both())
        );
        assert_eq!(
            parse_kernel_tables(&listing(&[nibrun("ip6", 4), nibrun("ip", 2)])),
            parse_kernel_tables(&holding_both())
        );
        let alongside = listing(&[
            r#"{"table": {"family": "inet", "name": "filter", "handle": 1}}"#.to_string(),
            nibrun("ip", 2),
            nibrun("ip6", 4),
        ]);
        assert_eq!(
            parse_kernel_tables(&alongside),
            parse_kernel_tables(&holding_both())
        );
        let bridged = listing(&[nibrun("ip", 2), nibrun("ip6", 4), nibrun("bridge", 6)]);
        assert_eq!(
            parse_kernel_tables(&bridged),
            parse_kernel_tables(&holding_both())
        );
    }

    #[test]
    fn output_that_cannot_be_read_is_a_host_holding_nothing() {
        assert_eq!(parse_kernel_tables("nft: command not found"), "");
        assert_eq!(parse_kernel_tables(r#"{"other": []}"#), "");
        assert_eq!(
            parse_kernel_tables(&listing(&[
                r#"{"table": {"family": "ip", "name": "nibrun"}}"#.to_string()
            ])),
            ""
        );
    }
}
