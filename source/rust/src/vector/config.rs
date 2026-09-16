//! Index-level `vectorSearch` configuration parsing.
//!
//! Move-only split of [`crate::vector`]: the config types
//! ([`HnswParams`], [`VectorAlgorithmKind`], [`VectorAlgorithm`],
//! [`VectorSearchConfig`]) and their parsers live here; the HNSW graph and
//! [`crate::vector::VectorEngine`] stay in the parent module.

use std::collections::BTreeMap;

use serde_json::Value;

use super::distance::Metric;

/// Defaults from the spec (`docs/implementation/phase_2_1_vector_indexing.md`): missing
/// kind-specific parameters objects fall back to these.
pub const DEFAULT_M: usize = 4;
pub const DEFAULT_EF_CONSTRUCTION: usize = 400;
pub const DEFAULT_EF_SEARCH: usize = 500;
/// HNSW constructor clamp: `hnsw_rs` supports at most 16 layers.
pub const HNSW_MAX_LAYER: usize = 16;
/// Azure limit: at most 16 vector fields per index.
pub const MAX_VECTOR_FIELDS: usize = 16;

/// HNSW construction/search parameters for one `hnsw` algorithm entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            m: DEFAULT_M,
            ef_construction: DEFAULT_EF_CONSTRUCTION,
            ef_search: DEFAULT_EF_SEARCH,
        }
    }
}

impl HnswParams {
    /// Parses the three HNSW fields from a kind-specific parameters object in
    /// one call. The REST docs use camelCase (`efConstruction`) while the
    /// pinned SDK serializes `snake_case` (`ef_construction`); both are
    /// accepted.
    ///
    /// # Errors
    ///
    /// Returns an error string when a value is not a positive integer, `m`
    /// is out of range, or an `ef` value is zero.
    fn parse(params: &serde_json::Map<String, Value>, algorithm: &str) -> Result<Self, String> {
        let m = parse_hnsw_usize(params, &["m"], DEFAULT_M, algorithm)?;
        // `Hnsw::new` calls `std::process::exit(1)` when
        // `max_nb_connection > 256`; reject before ever constructing.
        if m == 0 || m > 256 {
            return Err(format!(
                "Invalid \"m\" value on algorithm {algorithm:?}; must be 1-256."
            ));
        }
        let ef_construction = parse_hnsw_usize(
            params,
            &["efConstruction", "ef_construction"],
            DEFAULT_EF_CONSTRUCTION,
            algorithm,
        )?;
        let ef_search = parse_hnsw_usize(
            params,
            &["efSearch", "ef_search"],
            DEFAULT_EF_SEARCH,
            algorithm,
        )?;
        if ef_construction == 0 {
            return Err(format!(
                "Invalid \"efConstruction\" value on algorithm {algorithm:?}; must be a positive integer."
            ));
        }
        if ef_search == 0 {
            return Err(format!(
                "Invalid \"efSearch\" value on algorithm {algorithm:?}; must be a positive integer."
            ));
        }
        Ok(HnswParams {
            m,
            ef_construction,
            ef_search,
        })
    }
}

/// The `kind` of a `vectorSearch.algorithms[]` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorAlgorithmKind {
    Hnsw,
    ExhaustiveKnn,
}

/// One parsed `vectorSearch.algorithms[]` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorAlgorithm {
    pub kind: VectorAlgorithmKind,
    pub metric: Metric,
    pub params: HnswParams,
}

/// Parsed `vectorSearch`: algorithm name → config, profile name →
/// algorithm name, and profile name → vectorizer name (Phase 2.4). The
/// vectorizer is associated with a vector field through its profile in the
/// pinned SDKs' wire format (the field's `vectorSearchProfile` → the
/// profile's `vectorizer`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorSearchConfig {
    pub algorithms: BTreeMap<String, VectorAlgorithm>,
    pub profiles: BTreeMap<String, String>,
    /// Profile name → the `vectorizer` name the profile references, when set.
    pub profile_vectorizers: BTreeMap<String, String>,
}

/// Parses the index-level `vectorSearch` object (or its absence) into a
/// [`VectorSearchConfig`].
///
/// Accepts the documented REST shape (`hnswParameters` /
/// `exhaustiveKnnParameters`, `algorithmConfigurationName`) plus the
/// `snake_case` aliases the SDK may serialize (`parameters`,
/// `algorithm_configuration_name`). Unknown top-level keys on algorithm
/// entries are ignored for forward-compat; a missing kind-specific parameters
/// object falls back to defaults.
///
/// # Errors
///
/// Returns an error string when an algorithm entry is malformed, names are
/// duplicated, a metric/kind is unknown, `m` is out of range, or a profile
/// references an unknown algorithm. The caller surfaces these as `400
/// InvalidIndex`.
pub fn parse_vector_search(raw: Option<&Value>) -> Result<VectorSearchConfig, String> {
    let Some(obj) = raw else {
        return Ok(VectorSearchConfig::default());
    };
    let map = obj.as_object().ok_or_else(|| {
        "Index \"vectorSearch\" must be a JSON object with \"algorithms\" and \"profiles\" arrays."
            .to_owned()
    })?;
    let mut config = VectorSearchConfig::default();

    if let Some(algorithms) = map.get("algorithms").or_else(|| map.get("algorithm")) {
        let entries = algorithms
            .as_array()
            .ok_or_else(|| "Index \"vectorSearch.algorithms\" must be an array.".to_owned())?;
        for entry in entries {
            let (name, algorithm) = parse_algorithm(entry)?;
            if config.algorithms.insert(name.clone(), algorithm).is_some() {
                return Err(format!("Duplicate vector search algorithm name {name:?}."));
            }
        }
    }

    if let Some(profiles) = map.get("profiles").or_else(|| map.get("profile")) {
        let entries = profiles
            .as_array()
            .ok_or_else(|| "Index \"vectorSearch.profiles\" must be an array.".to_owned())?;
        for entry in entries {
            let obj = entry
                .as_object()
                .ok_or_else(|| "Vector search profile entries must be JSON objects.".to_owned())?;
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "Vector search profile is missing a non-empty \"name\".".to_owned()
                })?;
            let algorithm = obj
                .get("algorithmConfigurationName")
                .or_else(|| obj.get("algorithm_configuration_name"))
                .or_else(|| obj.get("algorithm"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    format!("Vector search profile {name:?} is missing its algorithm configuration name.")
                })?;
            if !config.algorithms.contains_key(algorithm) {
                return Err(format!(
                    "Vector search profile {name:?} references unknown algorithm {algorithm:?}."
                ));
            }
            if config
                .profiles
                .insert(name.to_owned(), algorithm.to_owned())
                .is_some()
            {
                return Err(format!("Duplicate vector search profile name {name:?}."));
            }
            // The profile's optional `vectorizer` reference (Phase 2.4): the
            // vectorizer is associated with a vector field through its profile.
            if let Some(vectorizer) = obj
                .get("vectorizer")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                config
                    .profile_vectorizers
                    .insert(name.to_owned(), vectorizer.to_owned());
            }
        }
    }

    Ok(config)
}

fn parse_algorithm(entry: &Value) -> Result<(String, VectorAlgorithm), String> {
    let obj = entry
        .as_object()
        .ok_or_else(|| "Vector search algorithm entries must be JSON objects.".to_owned())?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Vector search algorithm is missing a non-empty \"name\".".to_owned())?;
    let kind_raw = obj.get("kind").and_then(Value::as_str).unwrap_or("hnsw");
    let kind = match kind_raw {
        "hnsw" => VectorAlgorithmKind::Hnsw,
        "exhaustiveKnn" => VectorAlgorithmKind::ExhaustiveKnn,
        other => {
            return Err(format!(
                "Unknown vector search algorithm kind {other:?} on algorithm {name:?}; \
                 supported kinds: \"hnsw\", \"exhaustiveKnn\"."
            ))
        }
    };
    // Nested per-kind parameters; a missing object falls back to defaults.
    // `parameters` is accepted as an alias for the kind-specific object.
    let params_obj = match kind {
        VectorAlgorithmKind::Hnsw => obj
            .get("hnswParameters")
            .or_else(|| obj.get("parameters"))
            .and_then(Value::as_object),
        VectorAlgorithmKind::ExhaustiveKnn => obj
            .get("exhaustiveKnnParameters")
            .or_else(|| obj.get("parameters"))
            .and_then(Value::as_object),
    };
    let mut params = HnswParams::default();
    let mut metric = Metric::Cosine;
    if let Some(p) = params_obj {
        if let Some(metric_raw) = p.get("metric").and_then(Value::as_str) {
            metric = Metric::parse(metric_raw)
                .ok_or_else(|| format!("Unknown metric {metric_raw:?} on algorithm {name:?}."))?;
        }
        if matches!(kind, VectorAlgorithmKind::Hnsw) {
            params = HnswParams::parse(p, name)?;
        }
    }
    Ok((
        name.to_owned(),
        VectorAlgorithm {
            kind,
            metric,
            params,
        },
    ))
}

fn parse_hnsw_usize(
    params: &serde_json::Map<String, Value>,
    keys: &[&str],
    default: usize,
    algorithm: &str,
) -> Result<usize, String> {
    let key = keys[0];
    let value = keys.iter().find_map(|k| params.get(*k));
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| {
                format!(
                    "Invalid {key:?} value on algorithm {algorithm:?}; must be a positive integer."
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config_raw() -> Value {
        json!({
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
        })
    }

    #[test]
    fn parse_vector_search_validates() {
        let raw = test_config_raw();
        let config = parse_vector_search(Some(&raw)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(config.algorithms.len(), 2);
        assert_eq!(config.profiles.len(), 2);
        assert!(parse_vector_search(None)
            .unwrap_or_else(|e| panic!("{e}"))
            .algorithms
            .is_empty());

        // Unknown metric.
        let bad = json!({
            "algorithms": [{"name": "a", "kind": "hnsw",
                            "hnswParameters": {"metric": "manhattan"}}],
            "profiles": []
        });
        assert!(parse_vector_search(Some(&bad)).is_err());
        // Unknown kind.
        let bad = json!({
            "algorithms": [{"name": "a", "kind": "flat"}],
            "profiles": []
        });
        assert!(parse_vector_search(Some(&bad)).is_err());
        // Profile references unknown algorithm.
        let bad = json!({
            "algorithms": [],
            "profiles": [{"name": "p", "algorithmConfigurationName": "missing"}]
        });
        assert!(parse_vector_search(Some(&bad)).is_err());
        // Duplicate algorithm names.
        let bad = json!({
            "algorithms": [{"name": "a", "kind": "hnsw"}, {"name": "a", "kind": "hnsw"}],
            "profiles": []
        });
        assert!(parse_vector_search(Some(&bad)).is_err());
        // Missing parameters object falls back to defaults.
        let fallback = json!({
            "algorithms": [{"name": "a", "kind": "hnsw"}],
            "profiles": [{"name": "p", "algorithmConfigurationName": "a"}]
        });
        let config = parse_vector_search(Some(&fallback)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(config.algorithms["a"].params, HnswParams::default());
        assert_eq!(config.algorithms["a"].metric, Metric::Cosine);
        // The pinned SDK shape: `parameters` object with snake_case keys.
        let sdk_shape = json!({
            "algorithms": [{
                "name": "h", "kind": "hnsw",
                "parameters": {"m": 8, "ef_construction": 100,
                               "ef_search": 50, "metric": "euclidean"}
            }],
            "profiles": [{"name": "p", "algorithm_configuration_name": "h"}]
        });
        let config = parse_vector_search(Some(&sdk_shape)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(config.algorithms["h"].metric, Metric::Euclidean);
        assert_eq!(
            config.algorithms["h"].params,
            HnswParams {
                m: 8,
                ef_construction: 100,
                ef_search: 50
            }
        );
        assert_eq!(config.profiles["p"], "h");
    }
}
