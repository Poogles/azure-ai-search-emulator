//! Facet computation.

use serde_json::{Map, Value};

use crate::storage::Document;

use super::types::Facet;

/// Computes facet counts over the full (filtered, ordered) result set: one
/// entry per distinct value, ordered by count descending then value ascending,
/// truncated to the facet's limit when one is given. The special `$count`
/// facet reports the total number of documents in the result set.
pub(crate) fn compute_facets(documents: &[Document], facets: &[Facet]) -> Value {
    let mut map = Map::new();
    for facet in facets {
        if facet.field == "$count" {
            map.insert(
                facet.field.clone(),
                Value::from(u64::try_from(documents.len()).unwrap_or(u64::MAX)),
            );
            continue;
        }
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for document in documents {
            let Some(value) = document.fields.get(&facet.field) else {
                continue;
            };
            let values = if value.is_array() {
                value.as_array().cloned().unwrap_or_default()
            } else {
                vec![value.clone()]
            };
            for value in values {
                if value.is_null() {
                    continue;
                }
                let key = facet_key(&value);
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        let mut entries: Vec<(String, u64)> = counts.into_iter().collect();
        entries.sort_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
        if let Some(limit) = facet.limit {
            entries.truncate(limit);
        }
        let items = entries
            .into_iter()
            .map(|(key, count)| {
                Value::Object({
                    let mut entry = Map::new();
                    entry.insert("value".to_owned(), facet_value(&key));
                    entry.insert("count".to_owned(), Value::from(count));
                    entry
                })
            })
            .collect();
        map.insert(facet.field.clone(), Value::Array(items));
    }
    Value::Object(map)
}

fn facet_key(value: &Value) -> String {
    match value {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Bool(b) => format!("b:{b}"),
        other => format!("o:{other}"),
    }
}

fn facet_value(key: &str) -> Value {
    match key.split_once(':') {
        Some(("s", rest)) => Value::String(rest.to_owned()),
        Some(("n", rest)) => rest
            .parse::<i64>()
            .ok()
            .map(Value::from)
            .or_else(|| rest.parse::<f64>().ok().map(Value::from))
            .unwrap_or_else(|| Value::String(rest.to_owned())),
        Some(("b", rest)) => Value::Bool(rest == "true"),
        _ => Value::String(key.to_owned()),
    }
}
