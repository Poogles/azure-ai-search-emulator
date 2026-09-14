//! Facet computation.

use serde_json::{Map, Value};

use crate::storage::Document;

use super::types::Facet;

/// Computes facet counts over the full (filtered, ordered) scored set: one
/// entry per distinct value, ordered by count descending then value ascending,
/// truncated to the facet's limit when one is given. The special `$count`
/// facet reports the total number of documents in the result set. Takes the
/// scored `(document, score)` pairs by reference so callers avoid cloning the
/// matched set a second time; scores are ignored.
pub(crate) fn compute_facets(scored: &[(Document, f32)], facets: &[Facet]) -> Value {
    let mut map = Map::new();
    for facet in facets {
        if facet.field == "$count" {
            map.insert(
                facet.field.clone(),
                Value::from(u64::try_from(scored.len()).unwrap_or(u64::MAX)),
            );
            continue;
        }
        let mut counts: std::collections::BTreeMap<String, (FacetValue, u64)> =
            std::collections::BTreeMap::new();
        for (document, _) in scored {
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
                let entry = counts
                    .entry(key)
                    .or_insert_with(|| (FacetValue::classify(&value), 0));
                entry.1 += 1;
            }
        }
        let mut entries: Vec<(String, FacetValue, u64)> = counts
            .into_iter()
            .map(|(key, (value, count))| (key, value, count))
            .collect();
        entries.sort_by(|(a, _, ac), (b, _, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
        if let Some(limit) = facet.limit {
            entries.truncate(limit);
        }
        let items = entries
            .into_iter()
            .map(|(_, value, count)| {
                Value::Object({
                    let mut entry = Map::new();
                    entry.insert("value".to_owned(), value.to_value());
                    entry.insert("count".to_owned(), Value::from(count));
                    entry
                })
            })
            .collect();
        map.insert(facet.field.clone(), Value::Array(items));
    }
    Value::Object(map)
}

/// The sort/dedup key for a facet value: a type-prefixed string (`s:`/`n:`/
/// `b:`/`o:`) so distinct values are unique and equal-count entries order
/// deterministically. The emitted value itself is carried by [`FacetValue`],
/// not re-parsed from this key.
fn facet_key(value: &Value) -> String {
    match value {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Bool(b) => format!("b:{b}"),
        other => format!("o:{other}"),
    }
}

/// A distinct facet value, classified by JSON type so the emitted value is the
/// original (no lossy string round-trip: large integers keep their exact
/// magnitude and non-scalar values are emitted as-is).
#[derive(Clone)]
enum FacetValue {
    String(String),
    Number(serde_json::Number),
    Bool(bool),
    Other(Value),
}

impl FacetValue {
    fn classify(value: &Value) -> Self {
        match value {
            Value::String(s) => FacetValue::String(s.clone()),
            Value::Number(n) => FacetValue::Number(n.clone()),
            Value::Bool(b) => FacetValue::Bool(*b),
            other => FacetValue::Other(other.clone()),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            FacetValue::String(s) => Value::String(s.clone()),
            FacetValue::Number(n) => Value::Number(n.clone()),
            FacetValue::Bool(b) => Value::Bool(*b),
            FacetValue::Other(v) => v.clone(),
        }
    }
}
