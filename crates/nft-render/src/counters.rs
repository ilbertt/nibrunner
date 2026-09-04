use std::collections::BTreeMap;

use protocol::AppId;

use crate::firewall::APP_COUNTER_PREFIX;

/// What the kernel has counted against one app since the table it lives in was last written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppTraffic {
    pub packets: u64,
    pub bytes: u64,
}

/// `nft -j list counters` answers with one object per counter under a top-level `nftables` array,
/// mixed in with a `metainfo` entry. Everything here is defensive about shape rather than typed
/// against it: this is another process's output, and a counter that cannot be read is one app
/// whose activity is unknown rather than a reason to fail the pass that reads the rest.
pub fn parse_app_traffic(json: &str) -> BTreeMap<AppId, AppTraffic> {
    let mut traffic = BTreeMap::new();
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
        return traffic;
    };
    let Some(entries) = parsed.get("nftables").and_then(|v| v.as_array()) else {
        return traffic;
    };
    for entry in entries {
        let Some(counter) = entry.get("counter").and_then(|v| v.as_object()) else {
            continue;
        };
        let (Some(name), Some(packets), Some(bytes)) = (
            counter.get("name").and_then(|v| v.as_str()),
            counter.get("packets").and_then(|v| v.as_u64()),
            counter.get("bytes").and_then(|v| v.as_u64()),
        ) else {
            continue;
        };
        if let Some(app_id) = app_id_from(name) {
            traffic.insert(app_id, AppTraffic { packets, bytes });
        }
    }
    traffic
}

/// Counters this table holds that are not an app's are somebody else's to explain, so they are
/// skipped. The prefix is what attributes a counter, and the id rule only refuses a name no id
/// could be.
fn app_id_from(name: &str) -> Option<AppId> {
    name.strip_prefix(APP_COUNTER_PREFIX).and_then(|value| AppId::parse(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::app_counter_name;

    fn counters(entries: &[(&str, u64)]) -> String {
        let mut list = vec![serde_json::json!({ "metainfo": { "version": "1.0.4" } })];
        list.extend(entries.iter().map(|(name, bytes)| {
            serde_json::json!({ "counter": { "family": "ip", "name": name, "table": "nibrun", "handle": 2, "packets": 3, "bytes": bytes } })
        }));
        serde_json::json!({ "nftables": list }).to_string()
    }

    #[test]
    fn a_counter_is_attributed_by_its_name_and_every_app_is_read() {
        let app = AppId::parse("0198f3aa-1c2d-7e4b-9f11-a0b1c2d3e4f5").unwrap();
        let other = AppId::parse("0198f3bb-2d3e-7f5c-8a22-b1c2d3e4f5a6").unwrap();
        let traffic = parse_app_traffic(&counters(&[(&app_counter_name(&app), 512), (&app_counter_name(&other), 1024)]));
        assert_eq!(traffic.get(&app), Some(&AppTraffic { packets: 3, bytes: 512 }));
        assert_eq!(traffic.get(&other).map(|t| t.bytes), Some(1024));
        assert_eq!(traffic.len(), 2);
    }

    #[test]
    fn what_is_not_an_app_counter_is_skipped_rather_than_parsed() {
        assert!(parse_app_traffic(&counters(&[("something_else", 8)])).is_empty());
        assert!(parse_app_traffic(&counters(&[("app_has.a.dot", 8)])).is_empty());
        assert!(parse_app_traffic(&counters(&[(&format!("app_{}", "x".repeat(64)), 8)])).is_empty());
        assert!(parse_app_traffic("not json").is_empty());
        assert!(parse_app_traffic("{}").is_empty());
        assert!(parse_app_traffic(r#"{"nftables":[{"counter":{"name":5}}]}"#).is_empty());
    }
}
