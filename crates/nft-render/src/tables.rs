use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::firewall::{NFTABLES_FAMILIES, NFTABLES_TABLE};

#[derive(Default, serde::Serialize)]
struct Table {
    objects: BTreeMap<String, Value>,
    rules: BTreeMap<String, Vec<Value>>,
}

fn entry(value: &Value) -> Option<(&str, &Map<String, Value>)> {
    let object = value.get("add").unwrap_or(value).as_object()?;
    let (kind, body) = object.iter().next()?;
    let body = body.as_object()?;
    let owner = body
        .get(if kind == "table" { "name" } else { "table" })?
        .as_str()?;
    (owner == NFTABLES_TABLE).then_some((kind, body))
}

fn remember(table: &mut Table, kind: &str, body: &Map<String, Value>) -> Option<()> {
    let mut value = body.clone();
    if kind == "rule" {
        let chain = body.get("chain")?.as_str()?;
        table
            .rules
            .entry(chain.into())
            .or_default()
            .push(Value::Object(value));
    } else {
        if kind == "counter" {
            value.remove("packets");
            value.remove("bytes");
        }
        let name = body.get("name")?.as_str()?;
        table
            .objects
            .insert(format!("{kind}:{name}"), Value::Object(value));
    }
    Some(())
}

pub fn parse_kernel_ruleset(json: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(json).ok()?;
    let entries = parsed.get("nftables")?.as_array()?;
    let mut tables = BTreeMap::<String, Table>::new();
    for item in entries {
        let Some((kind, body)) = entry(item) else { continue };
        let family = body.get("family")?.as_str()?;
        if !NFTABLES_FAMILIES.contains(&family) {
            continue;
        }
        if kind == "table" {
            tables.insert(family.into(), Table::default());
        }
        remember(tables.get_mut(family)?, kind, body)?;
    }
    for family in NFTABLES_FAMILIES {
        let table = tables.get(family)?;
        for chain in ["input", "forward"] {
            if !table.objects.contains_key(&format!("chain:{chain}")) || table.rules.get(chain)?.is_empty() {
                return None;
            }
        }
    }
    serde_json::to_string(&tables).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn installed() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/installed.json")).unwrap()
    }
    fn fingerprint(value: &Value) -> Option<String> {
        parse_kernel_ruleset(&value.to_string())
    }
    #[test]
    fn rules_removed_without_replacing_their_tables_are_detected() {
        let mut value = installed();
        assert!(fingerprint(&value).is_some());
        value["nftables"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry.get("rule").is_none());
        assert!(fingerprint(&value).is_none());
    }
    #[test]
    fn the_echoed_transaction_matches_the_listing_even_with_a_replaced_table() {
        let value = installed();
        let entries = value["nftables"].as_array().unwrap();
        let mut echoed: Vec<Value> = entries
            .iter()
            .filter(|entry| entry.get("metainfo").is_none())
            .map(|entry| serde_json::json!({"add": entry}))
            .collect();
        echoed.insert(0, echoed[0].clone());
        assert_eq!(
            fingerprint(&value),
            fingerprint(&serde_json::json!({"nftables": echoed}))
        );
    }
    #[test]
    fn a_changed_verdict_or_rule_order_is_not_the_installed_firewall() {
        let value = installed();
        let changed = value.to_string().replace("\"reject\"", "\"accept\"");
        assert_ne!(fingerprint(&value), parse_kernel_ruleset(&changed));
        let mut reordered = installed();
        let entries = reordered["nftables"].as_array_mut().unwrap();
        let rules: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.get("rule").is_some())
            .map(|(index, _)| index)
            .collect();
        entries.swap(rules[0], rules[1]);
        assert_ne!(fingerprint(&value), fingerprint(&reordered));
    }
    #[test]
    fn malformed_output_and_missing_ipv6_never_verify() {
        for value in ["", "not json", "{}", r#"{"nftables":[]}"#] {
            assert!(parse_kernel_ruleset(value).is_none());
        }
        let mut value = installed();
        value["nftables"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| !entry.to_string().contains("\"ip6\""));
        assert!(fingerprint(&value).is_none());
    }
}
