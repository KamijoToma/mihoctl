//! YAML override merge with Clash Party / Sparkle operators:
//!
//! - nested maps merge recursively; arrays and scalars replace
//! - `key!`  replaces the value wholesale (no recursive merge)
//! - `+key`  prepends an array, `key+` appends an array
//! - `<key>` escapes a literal key that would otherwise look like an operator

use anyhow::{Result, bail};
use serde_json::{Map, Value};

pub fn deep_merge(target: &mut Map<String, Value>, other: Map<String, Value>) -> Result<()> {
    for (raw, value) in other {
        if let Some(k) = raw.strip_suffix('!') {
            target.insert(unwrap(k).to_string(), value);
            continue;
        }
        if value.is_array() {
            if let Some(k) = raw.strip_prefix('+') {
                splice(target, unwrap(k), value, true)?;
                continue;
            }
            if let Some(k) = raw.strip_suffix('+') {
                splice(target, unwrap(k), value, false)?;
                continue;
            }
        }
        let key = unwrap(&raw).to_string();
        match value {
            Value::Object(inner) => {
                let slot = target.entry(key).or_insert_with(|| Value::Object(Map::new()));
                if !slot.is_object() {
                    *slot = Value::Object(Map::new());
                }
                deep_merge(slot.as_object_mut().expect("just ensured object"), inner)?;
            }
            other => {
                target.insert(key, other);
            }
        }
    }
    Ok(())
}

fn unwrap(key: &str) -> &str {
    key.strip_prefix('<').and_then(|k| k.strip_suffix('>')).unwrap_or(key)
}

fn splice(target: &mut Map<String, Value>, key: &str, value: Value, prepend: bool) -> Result<()> {
    let Value::Array(mut items) = value else { unreachable!("caller checked array") };
    let slot = target.entry(key.to_string()).or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(existing) = slot else {
        bail!("cannot {} to `{key}`: existing value is not a list", if prepend { "prepend" } else { "append" });
    };
    if prepend {
        items.append(existing);
        *existing = items;
    } else {
        existing.append(&mut items);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merged(base: Value, over: Value) -> Value {
        let mut b = base.as_object().unwrap().clone();
        deep_merge(&mut b, over.as_object().unwrap().clone()).unwrap();
        Value::Object(b)
    }

    #[test]
    fn maps_merge_recursively_and_scalars_arrays_replace() {
        let out = merged(
            json!({"dns": {"enable": true, "ipv6": true, "nameserver": ["a", "b"]}, "mode": "rule"}),
            json!({"dns": {"ipv6": false, "nameserver": ["c"]}}),
        );
        assert_eq!(out, json!({"dns": {"enable": true, "ipv6": false, "nameserver": ["c"]}, "mode": "rule"}));
    }

    #[test]
    fn bang_replaces_map_wholesale() {
        let out = merged(json!({"sniffer": {"enable": true, "sniff": {"TLS": {}}}}), json!({"sniffer!": {"enable": false}}));
        assert_eq!(out, json!({"sniffer": {"enable": false}}));
    }

    #[test]
    fn plus_prefix_prepends_and_suffix_appends() {
        let out = merged(json!({"rules": ["MATCH,P"]}), json!({"+rules": ["A,1", "B,2"], "proxies+": [1]}));
        assert_eq!(out, json!({"rules": ["A,1", "B,2", "MATCH,P"], "proxies": [1]}));
        let out = merged(json!({"rules": ["X"]}), json!({"rules+": ["MATCH,P"]}));
        assert_eq!(out, json!({"rules": ["X", "MATCH,P"]}));
    }

    #[test]
    fn angle_brackets_escape_literal_keys() {
        let out = merged(json!({"hosts": {}}), json!({"hosts": {"<+.example.com>": "1.2.3.4"}}));
        assert_eq!(out, json!({"hosts": {"+.example.com": "1.2.3.4"}}));
    }

    #[test]
    fn splice_onto_non_list_is_an_error() {
        let mut b = json!({"rules": "oops"}).as_object().unwrap().clone();
        let err = deep_merge(&mut b, json!({"+rules": ["A"]}).as_object().unwrap().clone()).unwrap_err();
        assert!(err.to_string().contains("not a list"));
    }
}
