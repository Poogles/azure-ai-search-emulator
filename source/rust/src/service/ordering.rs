//! Score ordering and RRF fusion.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::storage::Document;
use crate::vector::sort_scored;

use super::types::OrderBy;

/// Fuses a full-text score list and per-vector-query score lists with weighted
/// Reciprocal Rank Fusion (RRF). Each list is ranked by score (descending, key
/// as tie-breaker); a document's fused score is the sum of
/// `weight * 1 / (k + rank)` over the lists in which it appears, where `rank`
/// is the 1-based position, `weight` is the list's weight, and `k` is the
/// standard RRF constant (60). A document appearing in several lists
/// accumulates all contributions, so it outranks documents in fewer lists, and
/// a heavier list contributes more.
pub(crate) fn rrf_fuse_weighted(
    full_text: &BTreeMap<String, f32>,
    full_text_weight: f32,
    vector_lists: &[(f32, BTreeMap<String, f32>)],
) -> BTreeMap<String, f32> {
    let mut fused: BTreeMap<String, f32> = BTreeMap::new();
    rrf_add_list(&mut fused, full_text_weight, full_text);
    for (weight, list) in vector_lists {
        rrf_add_list(&mut fused, *weight, list);
    }
    fused
}

/// Adds one ranked list's weighted RRF contribution to `fused`.
fn rrf_add_list(fused: &mut BTreeMap<String, f32>, weight: f32, list: &BTreeMap<String, f32>) {
    const K: f32 = 60.0;
    let mut ranked: Vec<(&String, &f32)> = list.iter().collect();
    ranked.sort_by(|x, y| y.1.total_cmp(x.1).then_with(|| x.0.cmp(y.0)));
    let mut rank = 1.0f32;
    for (key, _) in &ranked {
        let contribution = weight * (1.0 / (K + rank));
        *fused.entry((*key).clone()).or_insert(0.0) += contribution;
        rank += 1.0;
    }
}

/// Orders scored `(document, score)` pairs: by `orderby` when given,
/// otherwise by score descending with the key field as tie-breaker so ranking
/// is deterministic. Match-all (unscored) queries carry equal scores, so they
/// stay in key order via the tie-breaker.
pub(crate) fn order_scored(scored: &mut [(Document, f32)], orderby: &[OrderBy]) {
    if orderby.is_empty() {
        sort_scored(scored, |doc: &Document| doc.key.as_str());
    } else {
        scored.sort_by(|a, b| compare_scored(a, b, orderby));
    }
}

/// Compares two scored `(document, score)` pairs by the `orderby` clauses,
/// with the key field as the final tie-breaker so ordering is deterministic.
/// Missing values sort first in ascending order and last in descending order,
/// matching Azure's null ordering.
fn compare_scored(
    a: &(Document, f32),
    b: &(Document, f32),
    orderby: &[OrderBy],
) -> std::cmp::Ordering {
    for clause in orderby {
        let ordering = if clause.field == "@search.score" {
            let ordering = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
            if clause.descending {
                ordering.reverse()
            } else {
                ordering
            }
        } else {
            compare_field(&a.0, &b.0, &clause.field, clause.descending)
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    a.0.key.cmp(&b.0.key)
}

fn compare_field(a: &Document, b: &Document, field: &str, descending: bool) -> std::cmp::Ordering {
    let (av, bv) = (a.fields.get(field), b.fields.get(field));
    match (av, bv) {
        (None, None) => std::cmp::Ordering::Equal,
        // Azure sorts nulls first in ascending order (last in descending).
        (None, Some(_)) => {
            if descending {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        (Some(_), None) => {
            if descending {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (Some(av), Some(bv)) => {
            let ordering = compare_values(av, bv);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        }
    }
}

fn compare_values(av: &Value, bv: &Value) -> std::cmp::Ordering {
    match (av, bv) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        // Mixed types: order by type tag so the result is deterministic.
        _ => type_tag(av).cmp(&type_tag(bv)),
    }
}

fn type_tag(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_fuse_weighted_ranks_documents_in_both_lists_highest() {
        // "a" is top on both lists, "b" top on one, "c" top on the other.
        let full_text = BTreeMap::from([("a".to_owned(), 0.9f32), ("b".to_owned(), 0.5f32)]);
        let vector = BTreeMap::from([("a".to_owned(), 0.8f32), ("c".to_owned(), 0.7f32)]);
        let fused = rrf_fuse_weighted(&full_text, 1.0, &[(1.0, vector)]);
        // "a" appears in both lists (rank 1 + rank 1) → highest fused score.
        assert!(fused["a"] > fused["b"]);
        assert!(fused["a"] > fused["c"]);
        // "b" and "c" each appear once at rank 1 → equal.
        assert!((fused["b"] - fused["c"]).abs() < f32::EPSILON);
        // Union: all three keys present.
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn rrf_fuse_weighted_scales_heavier_lists_more() {
        // "a" is rank 1 on the full-text list; "b" is rank 1 on the vector
        // list. With equal weights they tie; weighting the vector list higher
        // lifts "b" above "a".
        let full_text = BTreeMap::from([("a".to_owned(), 0.9f32)]);
        let vector = BTreeMap::from([("b".to_owned(), 0.9f32)]);
        let equal = rrf_fuse_weighted(&full_text, 1.0, &[(1.0, vector.clone())]);
        assert!((equal["a"] - equal["b"]).abs() < f32::EPSILON);
        let weighted = rrf_fuse_weighted(&full_text, 1.0, &[(2.0, vector)]);
        assert!(weighted["b"] > weighted["a"]);
    }

    #[test]
    fn rrf_fuse_weighted_empty_side_is_noop_contribution() {
        let full_text = BTreeMap::from([("a".to_owned(), 0.9f32)]);
        let empty: BTreeMap<String, f32> = BTreeMap::new();
        let fused = rrf_fuse_weighted(&full_text, 1.0, &[(1.0, empty)]);
        assert_eq!(fused.len(), 1);
        // Single list, rank 1 → 1/(60+1).
        assert!((fused["a"] - 1.0 / 61.0).abs() < f32::EPSILON);
    }
}
