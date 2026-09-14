//! Vector search backed by [`hnsw_rs`](https://crates.io/crates/hnsw_rs).
//!
//! One [`VectorIndex`] per (emulator index, vector field) pair mirrors the
//! [`crate::query::SearchEngine`] pattern: the service layer owns a
//! [`VectorEngine`] alongside the full-text engine and orchestrates
//! vector-only and hybrid search.
//!
//! Design notes (see `docs/decisions/0004-vector-index.md`):
//! - `hnsw_rs` indexes have a fixed dimension, so fields cannot share.
//! - `hnsw_rs` has no reliable point-deletion API, so document delete (and
//!   vector-field removal on merge) rebuilds the affected per-field index
//!   from the surviving vectors. The raw `vectors` map is the source of
//!   truth; the HNSW graph is a derived cache rebuilt after every mutation
//!   batch. Acceptable at emulator scale.
//! - The exact path (`exhaustiveKnn` profiles and per-query `exhaustive:
//!   true`) never touches the HNSW graph: a direct linear scan over the
//!   stored `Vec<f32>` values.
//! - `preFilter` is a constrained brute-force scan over filter-matching
//!   candidates (exact). The HNSW predicate path is a documented future
//!   optimization, not needed at emulator scale.
//! - Cosine uses `anndists`'s `DistCosine` directly (it normalizes internally
//!   via `1 - dot/(|a||b|)`), so no pre-normalization is needed.
//! - Dot product never touches the graph: `hnsw_rs` requires non-negative
//!   distances (it asserts `dist_to_ref >= 0` internally) and `anndists`'s
//!   `DistDot` asserts `dot <= 1`, so no wrapper can represent raw inner
//!   products over unnormalized vectors. `dotProduct` always executes as an
//!   exact brute-force scan, regardless of algorithm kind.

pub mod config;
pub mod distance;

use std::collections::BTreeMap;

use hnsw_rs::prelude::{DistCosine, DistL2, Distance, Hnsw};
use serde_json::Value;

use crate::sync_util::{read_unpoisoned, write_unpoisoned};

pub use config::{
    parse_vector_search, HnswParams, VectorAlgorithm, VectorAlgorithmKind, VectorSearchConfig,
    DEFAULT_EF_CONSTRUCTION, DEFAULT_EF_SEARCH, DEFAULT_M, HNSW_MAX_LAYER, MAX_VECTOR_FIELDS,
};
pub use distance::{brute_force_score, score_from_distance, Metric};

/// Allocation hint for a fresh per-field graph.
const HNSW_MAX_ELEMENTS_HINT: usize = 128;

/// One HNSW graph over a single vector field. Only cosine and euclidean have
/// graph backends: dotProduct always scans (see the module docs), so there
/// is no `Dot` variant — [`HnswBackend::build`] returns `None` for it.
enum HnswBackend {
    Cosine(Hnsw<'static, f32, DistCosine>),
    Euclidean(Hnsw<'static, f32, DistL2>),
}

/// Dispatches an identical method call to the inner HNSW graph, whichever
/// distance kernel it uses. The two variants expose the same API on distinct
/// types, so the match lives here once instead of at every call site.
macro_rules! with_hnsw {
    ($backend:expr, $hnsw:ident => $body:expr) => {
        match $backend {
            HnswBackend::Cosine($hnsw) => $body,
            HnswBackend::Euclidean($hnsw) => $body,
        }
    };
}

impl HnswBackend {
    /// Builds the graph backend for `metric`, or `None` for `dotProduct`
    /// (unrepresentable as a non-negative HNSW distance; always scanned).
    fn build(metric: Metric, params: HnswParams, capacity_hint: usize) -> Option<Self> {
        let max_elements = capacity_hint.max(HNSW_MAX_ELEMENTS_HINT);
        match metric {
            Metric::Cosine => Some(HnswBackend::Cosine(build_graph(params, max_elements))),
            Metric::DotProduct => None,
            Metric::Euclidean => Some(HnswBackend::Euclidean(build_graph(params, max_elements))),
        }
    }

    fn insert(&self, vector: &[f32], id: usize) {
        with_hnsw!(self, hnsw => hnsw.insert((vector, id)));
    }

    /// Approximate top-`k` (at most) as `(external id, distance)` pairs.
    fn search(&self, query: &[f32], k: usize, ef: usize) -> Vec<(usize, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let neighbours = with_hnsw!(self, hnsw => hnsw.search(query, k, ef));
        neighbours
            .iter()
            .map(|n| (n.get_origin_id(), n.get_distance()))
            .collect()
    }

    fn len(&self) -> usize {
        with_hnsw!(self, hnsw => hnsw.get_nb_point())
    }
}

/// Builds a fresh HNSW graph with the small-dataset remedies enabled
/// (`extend_candidates` + `keeping_pruned`, `hnsw_rs`'s own fixes for the
/// pruning heuristic, which can otherwise make it difficult to return the
/// requested number of neighbours on small data).
fn build_graph<D>(params: HnswParams, max_elements: usize) -> Hnsw<'static, f32, D>
where
    D: Distance<f32> + Send + Sync + Default,
{
    let mut hnsw = Hnsw::new(
        params.m,
        max_elements,
        HNSW_MAX_LAYER,
        params.ef_construction,
        D::default(),
    );
    hnsw.set_extend_candidates(true);
    hnsw.set_keeping_pruned(true);
    hnsw
}

/// A per-field vector index: the raw vectors (source of truth) plus an
/// optional HNSW graph derived from them (`None` for `exhaustiveKnn`
/// profiles and for `dotProduct` metrics, which always scan).
struct VectorIndex {
    metric: Metric,
    params: HnswParams,
    dimensions: usize,
    use_hnsw: bool,
    vectors: BTreeMap<String, Vec<f32>>,
    /// External `DataId` → document key, rebuilt with the graph.
    id_to_key: Vec<String>,
    hnsw: Option<HnswBackend>,
}

impl VectorIndex {
    fn new(metric: Metric, params: HnswParams, dimensions: usize, use_hnsw: bool) -> Self {
        VectorIndex {
            metric,
            params,
            dimensions,
            use_hnsw,
            vectors: BTreeMap::new(),
            id_to_key: Vec::new(),
            hnsw: None,
        }
    }

    /// Inserts or replaces `(key, vector)` entries, rebuilding the graph
    /// once for the whole batch.
    ///
    /// # Errors
    ///
    /// Returns an error string when a vector's length does not match the
    /// field's declared dimensions. No entries are inserted when any entry
    /// is invalid.
    fn upsert_many(&mut self, entries: &[(&str, &[f32])]) -> Result<(), String> {
        for (key, vector) in entries {
            if vector.len() != self.dimensions {
                return Err(format!(
                    "Vector for {key:?} has dimension {}, expected {}.",
                    vector.len(),
                    self.dimensions
                ));
            }
        }
        for (key, vector) in entries {
            self.vectors.insert(key.to_string(), vector.to_vec());
        }
        self.rebuild();
        Ok(())
    }

    /// Removes the given keys, rebuilding the graph once when anything was
    /// removed.
    fn remove_many(&mut self, keys: &[&str]) {
        let mut removed = false;
        for key in keys {
            if self.vectors.remove(*key).is_some() {
                removed = true;
            }
        }
        if removed {
            self.rebuild();
        }
    }

    /// Rebuilds the HNSW graph (and the `DataId` → key map) from the raw
    /// vectors. External IDs are assigned in key order, so a rebuild is
    /// deterministic. No graph is built for `exhaustiveKnn` profiles or
    /// `dotProduct` metrics ([`HnswBackend::build`] returns `None` for the
    /// latter); those always scan.
    fn rebuild(&mut self) {
        let backend = if self.use_hnsw {
            HnswBackend::build(self.metric, self.params, self.vectors.len())
        } else {
            None
        };
        let Some(backend) = backend else {
            self.hnsw = None;
            self.id_to_key.clear();
            return;
        };
        let mut id_to_key = Vec::with_capacity(self.vectors.len());
        for (id, (key, vector)) in self.vectors.iter().enumerate() {
            backend.insert(vector, id);
            id_to_key.push(key.clone());
        }
        self.id_to_key = id_to_key;
        self.hnsw = Some(backend);
    }

    /// Top-`k` `(key, score)` pairs, score descending with key tie-break.
    ///
    /// The HNSW path is used only when there is no pre-filter, the query is
    /// not exhaustive, a graph exists (`hnsw` profile with a cosine or
    /// euclidean metric — `exhaustiveKnn` and `dotProduct` never build one),
    /// and the index is larger than the effective `ef` window. When the whole
    /// index fits inside the `ef` candidate window a scan costs no more than
    /// graph traversal and is exact, so small indexes (the emulator norm)
    /// always scan: deterministic results with no RNG-dependent recall
    /// flakes. Every other case scans for the same reason.
    fn search(
        &self,
        query: &[f32],
        k: usize,
        exhaustive: bool,
        pre_filter: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<(String, f32)> {
        // `ef` must cover `k` or the graph cannot return `k` neighbours;
        // never lower the configured value silently.
        let ef = self.params.ef_search.max(k);
        if pre_filter.is_some() || exhaustive || self.hnsw.is_none() || self.vectors.len() <= ef {
            return self.brute_force(query, k, pre_filter);
        }
        self.hnsw_search(query, k, ef)
    }

    fn hnsw_search(&self, query: &[f32], k: usize, ef: usize) -> Vec<(String, f32)> {
        let Some(backend) = &self.hnsw else {
            return Vec::new();
        };
        let scored: Vec<(String, f32)> = backend
            .search(query, k, ef)
            .into_iter()
            .filter_map(|(id, distance)| {
                self.id_to_key
                    .get(id)
                    .map(|key| (key.clone(), score_from_distance(self.metric, distance)))
            })
            .collect();
        top_k(scored, k)
    }

    fn brute_force(
        &self,
        query: &[f32],
        k: usize,
        pre_filter: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<(String, f32)> {
        let scored: Vec<(String, f32)> = self
            .vectors
            .iter()
            .filter(|(key, _)| pre_filter.is_none_or(|f| f(key)))
            .map(|(key, vector)| (key.clone(), brute_force_score(self.metric, query, vector)))
            .collect();
        top_k(scored, k)
    }

    fn len(&self) -> usize {
        self.vectors.len()
    }
}

/// Sorts `(item, score)` pairs by score descending, with `key_of(item)`
/// ascending as the tie-breaker, so ranking is deterministic. `NaN` scores
/// cannot occur (inputs are validated finite), so a fallback to `Equal` is
/// safe.
pub fn sort_scored<T>(scored: &mut [(T, f32)], key_of: impl Fn(&T) -> &str) {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| key_of(&a.0).cmp(key_of(&b.0)))
    });
}

/// Sorts `(key, score)` pairs by score (descending, key as tie-breaker) and
/// keeps only the top `k`.
fn top_k(mut scored: Vec<(String, f32)>, k: usize) -> Vec<(String, f32)> {
    sort_scored(&mut scored, String::as_str);
    scored.truncate(k);
    scored
}

/// Groups `(field, item)` pairs by field name: a `BTreeMap` of field to its
/// items, fields sorted, item order preserved within each field.
fn group_by_field<'a, T>(
    items: impl IntoIterator<Item = (&'a str, T)>,
) -> BTreeMap<&'a str, Vec<T>> {
    let mut by_field: BTreeMap<&'a str, Vec<T>> = BTreeMap::new();
    for (field, item) in items {
        by_field.entry(field).or_default().push(item);
    }
    by_field
}

/// The per-service vector store: one [`VectorIndex`] per (index, field).
///
/// Safe for concurrent use: searches take a read lock, mutations take a
/// write lock, mirroring [`crate::query::SearchEngine`].
#[derive(Default)]
pub struct VectorEngine {
    inner: std::sync::RwLock<BTreeMap<String, BTreeMap<String, VectorIndex>>>,
}

impl VectorEngine {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the per-field vector indexes for an emulator index.
    ///
    /// `fields` is `(field name, dimensions, profile name)` per vector field;
    /// each profile is resolved through `vector_search` to its algorithm's
    /// metric, kind, and parameters.
    ///
    /// # Errors
    ///
    /// Returns an error string (surfaced as `400 InvalidIndex`) when a
    /// profile is unknown, the index has more than 16 vector fields, or the
    /// `vectorSearch` configuration is malformed.
    pub fn create_index(
        &self,
        index: &str,
        vector_search: Option<&Value>,
        fields: &[(String, usize, String)],
    ) -> Result<(), String> {
        if fields.len() > MAX_VECTOR_FIELDS {
            return Err(format!(
                "Index {index:?} has {} vector fields; at most {MAX_VECTOR_FIELDS} are supported.",
                fields.len()
            ));
        }
        let config = parse_vector_search(vector_search)?;
        let mut per_field = BTreeMap::new();
        for (field, dimensions, profile) in fields {
            let algorithm_name = config.profiles.get(profile).ok_or_else(|| {
                format!(
                    "Vector field {field:?} references unknown vector search profile {profile:?}."
                )
            })?;
            let algorithm = config.algorithms.get(algorithm_name).ok_or_else(|| {
                format!(
                    "Vector search profile {profile:?} references unknown algorithm {algorithm_name:?}."
                )
            })?;
            if *dimensions == 0 {
                return Err(format!("Vector field {field:?} has invalid dimensions 0."));
            }
            per_field.insert(
                field.clone(),
                VectorIndex::new(
                    algorithm.metric,
                    algorithm.params,
                    *dimensions,
                    matches!(algorithm.kind, VectorAlgorithmKind::Hnsw),
                ),
            );
        }
        let mut guard = write_unpoisoned(&self.inner);
        guard.insert(index.to_owned(), per_field);
        Ok(())
    }

    /// Drops all vector state for an emulator index.
    pub fn delete_index(&self, index: &str) {
        let mut guard = write_unpoisoned(&self.inner);
        guard.remove(index);
    }

    /// Upserts `(key, field, vector)` entries, rebuilding each touched
    /// field's graph once. Entries for unknown index/field pairs are ignored
    /// (the service validates documents before calling).
    ///
    /// # Errors
    ///
    /// Returns an error string when a vector's length mismatches the field's
    /// declared dimensions.
    pub fn upsert_documents(
        &self,
        index: &str,
        entries: &[(String, String, Vec<f32>)],
    ) -> Result<(), String> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut guard = write_unpoisoned(&self.inner);
        let Some(per_field) = guard.get_mut(index) else {
            return Ok(());
        };
        // Group entries by field so each touched field's graph is rebuilt
        // once for the whole batch.
        let by_field = group_by_field(
            entries
                .iter()
                .map(|(key, field, vector)| (field.as_str(), (key.as_str(), vector.as_slice()))),
        );
        for (field, field_entries) in by_field {
            if let Some(vector_index) = per_field.get_mut(field) {
                vector_index.upsert_many(&field_entries)?;
            }
        }
        Ok(())
    }

    /// Removes `(key, field)` pairs (used when a full-replace upload drops a
    /// vector a document previously had). Unknown pairs are ignored.
    pub fn remove_entries(&self, index: &str, entries: &[(String, String)]) {
        if entries.is_empty() {
            return;
        }
        let mut guard = write_unpoisoned(&self.inner);
        let Some(per_field) = guard.get_mut(index) else {
            return;
        };
        let by_field = group_by_field(
            entries
                .iter()
                .map(|(key, field)| (field.as_str(), key.as_str())),
        );
        for (field, keys) in by_field {
            if let Some(vector_index) = per_field.get_mut(field) {
                vector_index.remove_many(&keys);
            }
        }
    }

    /// Removes keys from every vector field of the index, rebuilding each
    /// touched graph once.
    pub fn delete_documents(&self, index: &str, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let mut guard = write_unpoisoned(&self.inner);
        let Some(per_field) = guard.get_mut(index) else {
            return;
        };
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        for vector_index in per_field.values_mut() {
            vector_index.remove_many(&key_refs);
        }
    }

    /// Top-`k` `(key, score)` pairs for one (index, field), score descending.
    /// Returns an empty vec for unknown index/field pairs.
    pub fn search(
        &self,
        index: &str,
        field: &str,
        query: &[f32],
        k: usize,
        exhaustive: bool,
        pre_filter: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<(String, f32)> {
        let guard = read_unpoisoned(&self.inner);
        guard
            .get(index)
            .and_then(|per_field| per_field.get(field))
            .map_or_else(Vec::new, |vector_index| {
                vector_index.search(query, k, exhaustive, pre_filter)
            })
    }

    /// Returns the number of indexed vectors for one (index, field), or
    /// `None` for unknown pairs. Used in tests.
    #[must_use]
    pub fn len(&self, index: &str, field: &str) -> Option<usize> {
        let guard = read_unpoisoned(&self.inner);
        guard
            .get(index)
            .and_then(|per_field| per_field.get(field))
            .map(VectorIndex::len)
    }

    /// Returns the number of points in the HNSW graph for one (index,
    /// field): `None` for unknown pairs and for fields without a graph
    /// (`exhaustiveKnn` profiles, `dotProduct` metrics). Used in tests.
    #[must_use]
    pub fn hnsw_len(&self, index: &str, field: &str) -> Option<usize> {
        let guard = read_unpoisoned(&self.inner);
        guard
            .get(index)
            .and_then(|per_field| per_field.get(field))
            .and_then(|vector_index| vector_index.hnsw.as_ref().map(HnswBackend::len))
    }

    /// Clears all vector indexes.
    pub fn reset(&self) {
        let mut guard = write_unpoisoned(&self.inner);
        guard.clear();
    }
}

/// FNV-1a 64-bit hash of the canonical `(vectorQueries, vectorFilterMode)`
/// pair, bound into continuation tokens so a mid-paging vector-query change
/// is rejected with `400 InvalidQuery`.
#[must_use]
pub fn vector_query_hash(vector_queries_raw: &Value, filter_mode: &str) -> u64 {
    let canonical = serde_json::to_string(&(vector_queries_raw, filter_mode)).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> (Option<Value>, Vec<(String, usize, String)>) {
        let raw = json!({
            "algorithms": [
                {
                    "name": "hnsw-1",
                    "kind": "hnsw",
                    "hnswParameters": {
                        "m": 4,
                        "efConstruction": 40,
                        "efSearch": 20,
                        "metric": "cosine"
                    }
                },
                {
                    "name": "eknn-1",
                    "kind": "exhaustiveKnn",
                    "exhaustiveKnnParameters": { "metric": "dotProduct" }
                }
            ],
            "profiles": [
                { "name": "cos", "algorithmConfigurationName": "hnsw-1" },
                { "name": "dot", "algorithmConfigurationName": "eknn-1" }
            ]
        });
        let fields = vec![
            ("content_vector".to_owned(), 3, "cos".to_owned()),
            ("dot_vector".to_owned(), 2, "dot".to_owned()),
        ];
        (Some(raw), fields)
    }

    fn engine_with_docs() -> VectorEngine {
        let engine = VectorEngine::new();
        let (raw, fields) = test_config();
        engine
            .create_index("items", raw.as_ref(), &fields)
            .unwrap_or_else(|e| panic!("create_index failed: {e}"));
        engine
            .upsert_documents(
                "items",
                &[
                    (
                        "1".to_owned(),
                        "content_vector".to_owned(),
                        vec![1.0, 0.0, 0.0],
                    ),
                    (
                        "2".to_owned(),
                        "content_vector".to_owned(),
                        vec![0.0, 1.0, 0.0],
                    ),
                    (
                        "3".to_owned(),
                        "content_vector".to_owned(),
                        vec![0.0, 0.0, 1.0],
                    ),
                    ("1".to_owned(), "dot_vector".to_owned(), vec![2.0, 0.0]),
                    ("2".to_owned(), "dot_vector".to_owned(), vec![0.0, 3.0]),
                ],
            )
            .unwrap_or_else(|e| panic!("upsert failed: {e}"));
        engine
    }

    #[test]
    fn hnsw_search_returns_nearest_first() {
        let engine = engine_with_docs();
        let hits = engine.search("items", "content_vector", &[1.0, 0.0, 0.0], 2, false, None);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "1");
        // Scores descend.
        for pair in hits.windows(2) {
            assert!(pair[0].1 >= pair[1].1);
        }
        assert_eq!(engine.len("items", "content_vector"), Some(3));
        assert_eq!(engine.hnsw_len("items", "content_vector"), Some(3));
        // ExhaustiveKnn field has no graph.
        assert_eq!(engine.hnsw_len("items", "dot_vector"), None);
    }

    #[test]
    fn exhaustive_matches_brute_force_order() {
        let engine = engine_with_docs();
        let approx = engine.search("items", "content_vector", &[0.6, 0.6, 0.0], 3, false, None);
        let exact = engine.search("items", "content_vector", &[0.6, 0.6, 0.0], 3, true, None);
        // On this tiny fixture both paths agree on the top hit and ordering.
        assert_eq!(approx[0].0, exact[0].0);
        // ExhaustiveKnn field always scans.
        let hits = engine.search("items", "dot_vector", &[1.0, 1.0], 2, false, None);
        assert_eq!(hits[0].0, "2");
        assert!(hits[0].1 > hits[1].1);
    }

    #[test]
    fn pre_filter_constrains_candidates() {
        let engine = engine_with_docs();
        let hits = engine.search(
            "items",
            "content_vector",
            &[1.0, 0.0, 0.0],
            5,
            false,
            Some(&|key: &str| key != "1"),
        );
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|(key, _)| key != "1"));
    }

    #[test]
    fn delete_rebuilds_and_deleted_keys_never_match() {
        let engine = engine_with_docs();
        engine.delete_documents("items", &["1".to_owned()]);
        let hits = engine.search("items", "content_vector", &[1.0, 0.0, 0.0], 5, false, None);
        assert!(hits.iter().all(|(key, _)| key != "1"));
        assert_eq!(engine.len("items", "content_vector"), Some(2));
        assert_eq!(engine.hnsw_len("items", "content_vector"), Some(2));
        // Re-upserting the key makes it searchable again.
        engine
            .upsert_documents(
                "items",
                &[(
                    "1".to_owned(),
                    "content_vector".to_owned(),
                    vec![1.0, 0.0, 0.0],
                )],
            )
            .unwrap_or_else(|e| panic!("upsert failed: {e}"));
        let hits = engine.search("items", "content_vector", &[1.0, 0.0, 0.0], 5, false, None);
        assert_eq!(hits[0].0, "1");
    }

    #[test]
    fn dimension_mismatch_rejected() {
        let engine = engine_with_docs();
        let result = engine.upsert_documents(
            "items",
            &[("9".to_owned(), "content_vector".to_owned(), vec![1.0, 2.0])],
        );
        assert!(result.is_err());
    }

    #[test]
    fn concurrent_insert_and_search_do_not_corrupt() {
        use std::sync::{Arc, Barrier};

        let engine = Arc::new(engine_with_docs());
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for worker in 0..4 {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..10 {
                    let key = format!("w{worker}-{i}");
                    engine
                        .upsert_documents(
                            "items",
                            &[(key, "content_vector".to_owned(), vec![0.1, 0.2, 0.3])],
                        )
                        .unwrap_or_else(|e| panic!("upsert failed: {e}"));
                }
            }));
        }
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..10 {
                    let hits =
                        engine.search("items", "content_vector", &[0.1, 0.2, 0.3], 3, false, None);
                    assert!(hits.len() <= 3);
                }
            }));
        }
        for handle in handles {
            handle
                .join()
                .unwrap_or_else(|e| panic!("worker panicked: {e:?}"));
        }
    }

    /// Deterministic pseudo-random points (LCG): stable fixtures without a
    /// test-only RNG dependency.
    fn pseudo_points(count: usize, dimensions: usize) -> Vec<Vec<f32>> {
        let mut state: u64 = 0x1234_5678_9abc_def1;
        let mut next = move || {
            state = state
                .wrapping_mul(0x5851_f42d_4c95_7f2d)
                .wrapping_add(0x1405_7b7e_f767_814f);
            // Map to [-1, 1). The shifted state fits in 32 bits (exactly
            // representable in `f64`); the final narrow to `f32` is the
            // point of the generator.
            #[allow(clippy::cast_precision_loss)]
            let unit = (state >> 33) as f64 / (f64::from(u32::MAX) + 1.0);
            #[allow(clippy::cast_possible_truncation)]
            let mapped = (unit * 2.0 - 1.0) as f32;
            mapped
        };
        (0..count)
            .map(|_| (0..dimensions).map(|_| next()).collect())
            .collect()
    }

    #[test]
    fn hnsw_backend_search_returns_valid_bounded_hits() {
        // The graph path directly (bypassing the small-index scan fallback).
        // HNSW is approximate by nature — recall is covered deterministically
        // through the engine's exact scan path — so this test asserts only
        // structural properties: bounded count, valid ids, finite distances.
        // (`dotProduct` has no graph backend and is covered by
        // `dot_product_hnsw_kind_scans_exactly` below.)
        for metric in [Metric::Cosine, Metric::Euclidean] {
            let points = pseudo_points(120, 8);
            let backend = HnswBackend::build(metric, HnswParams::default(), points.len())
                .unwrap_or_else(|| panic!("{metric:?} must build a graph backend"));
            assert_eq!(backend.len(), 0);
            assert!(backend.search(&points[0], 5, 50).is_empty());
            for (id, point) in points.iter().enumerate() {
                backend.insert(point, id);
            }
            assert_eq!(backend.len(), points.len());
            for probe in [0, 17, 119] {
                let hits = backend.search(&points[probe], 5, 50);
                assert!(!hits.is_empty());
                assert!(hits.len() <= 5);
                for (id, distance) in &hits {
                    assert!(*id < points.len());
                    assert!(distance.is_finite());
                    assert!(score_from_distance(metric, *distance).is_finite());
                }
            }
        }
    }

    #[test]
    fn dot_product_hnsw_kind_scans_exactly() {
        // A `dotProduct` metric on an `hnsw` algorithm never builds a graph
        // (raw dots are unrepresentable as non-negative HNSW distances) and
        // always scans: exact ordering, including negative dots and large
        // unnormalized magnitudes that would panic any graph wrapper.
        let engine = VectorEngine::new();
        let raw = serde_json::json!({
            "algorithms": [
                {"name": "hnsw-dot", "kind": "hnsw",
                 "hnswParameters": {"m": 4, "efConstruction": 40,
                                    "efSearch": 20, "metric": "dotProduct"}}
            ],
            "profiles": [
                {"name": "dot", "algorithmConfigurationName": "hnsw-dot"}
            ]
        });
        engine
            .create_index(
                "items",
                Some(&raw),
                &[("v".to_owned(), 2, "dot".to_owned())],
            )
            .unwrap_or_else(|e| panic!("create_index failed: {e}"));
        engine
            .upsert_documents(
                "items",
                &[
                    ("neg".to_owned(), "v".to_owned(), vec![-3.0, 0.0]),
                    ("zero".to_owned(), "v".to_owned(), vec![0.0, 0.0]),
                    ("big".to_owned(), "v".to_owned(), vec![100.0, 100.0]),
                    ("unit".to_owned(), "v".to_owned(), vec![1.0, 0.0]),
                ],
            )
            .unwrap_or_else(|e| panic!("upsert failed: {e}"));
        assert_eq!(engine.hnsw_len("items", "v"), None);
        let hits = engine.search("items", "v", &[1.0, 1.0], 4, false, None);
        let keys: Vec<&str> = hits.iter().map(|(key, _)| key.as_str()).collect();
        // Dots: big=200, unit=1, zero=0, neg=-3.
        assert_eq!(keys, vec!["big", "unit", "zero", "neg"]);
        assert!(hits[0].1 > hits[1].1);
    }

    #[test]
    fn vector_query_hash_is_stable() {
        let raw = json!([{"kind": "vector", "k": 3}]);
        assert_eq!(
            vector_query_hash(&raw, "postFilter"),
            vector_query_hash(&raw, "postFilter")
        );
        assert_ne!(
            vector_query_hash(&raw, "postFilter"),
            vector_query_hash(&raw, "preFilter")
        );
    }
}
