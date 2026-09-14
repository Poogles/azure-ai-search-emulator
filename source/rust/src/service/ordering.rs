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
#[allow(clippy::cast_precision_loss)]
fn rrf_add_list(fused: &mut BTreeMap<String, f32>, weight: f32, list: &BTreeMap<String, f32>) {
    const K: f32 = 60.0;
    let mut ranked: Vec<(&String, &f32)> = list.iter().collect();
    ranked.sort_by(|x, y| y.1.total_cmp(x.1).then_with(|| x.0.cmp(y.0)));
    // Ranks are 1-based positions in a result list, far below the 2^24
    // exact-integer limit of `f32`, so the cast is lossless in practice.
    for (index, (key, _)) in ranked.iter().enumerate() {
        let rank = index + 1;
        let contribution = weight * (1.0 / (K + rank as f32));
        *fused.entry((*key).clone()).or_insert(0.0) += contribution;
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

/// Compares two documents on `field` by a comparable sort key. A missing
/// value sorts first in ascending order and last in descending order (the
/// `Option` key orders `None` below `Some`, and `descending` reverses it),
/// matching Azure's null ordering.
fn compare_field(a: &Document, b: &Document, field: &str, descending: bool) -> std::cmp::Ordering {
    let ordering = sort_key(a.fields.get(field)).cmp(&sort_key(b.fields.get(field)));
    if descending {
        ordering.reverse()
    } else {
        ordering
    }
}

/// A comparable key for a document field value. The `Option` wrapper puts
/// missing values first (ascending); the inner [`SortKey`] orders present
/// values by type tag, then by value within a type.
fn sort_key(value: Option<&Value>) -> Option<SortKey> {
    value.map(|value| match value {
        Value::Null => SortKey::Null,
        Value::Bool(b) => SortKey::Bool(*b),
        Value::Number(n) => SortKey::Number(number_key(n)),
        Value::String(s) => SortKey::String(s.clone()),
        Value::Array(_) => SortKey::Array,
        Value::Object(_) => SortKey::Object,
    })
}

/// The comparable form of a JSON value. Variant order is the type-tag order
/// used for mixed-type comparisons (`Null < Bool < Number < String < Array <
/// Object`); within a type the payload orders by value.
#[derive(PartialOrd, Ord, PartialEq, Eq)]
enum SortKey {
    Null,
    Bool(bool),
    Number(NumberKey),
    String(String),
    Array,
    Object,
}

/// A comparable key for a JSON number. Ordered by the `f64` value, with the
/// exact integer (as `i128`) as a tie-breaker so large magnitudes that round
/// to the same `f64` still order exactly. `f64` conversion is monotonic, so
/// this matches comparing integers in their native `i64`/`u64` form and
/// falling back to `f64` for floats and mixed pairs.
#[derive(Clone, Copy)]
struct NumberKey {
    f64: f64,
    int: Option<i128>,
}

impl PartialEq for NumberKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for NumberKey {}

impl PartialOrd for NumberKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NumberKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.f64
            .total_cmp(&other.f64)
            .then_with(|| self.int.cmp(&other.int))
    }
}

fn number_key(n: &serde_json::Number) -> NumberKey {
    let int = n
        .as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from));
    NumberKey {
        f64: n.as_f64().unwrap_or(f64::NAN),
        int,
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
