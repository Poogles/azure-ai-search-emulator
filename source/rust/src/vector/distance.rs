//! Distance metrics for vector search.
//!
//! [`Metric`] is the emulator's copy of the Azure `metric` property on a
//! vector-search algorithm entry (`cosine`, `dotProduct`, `euclidean`).
//! Score derivation is emulator-defined (see
//! `docs/phase_2_1_vector_indexing.md` §Scoring):
//! - cosine: `1 - cosine_distance`
//! - dotProduct: the raw inner product
//! - euclidean: `1 / (1 + l2_distance)`

/// A vector similarity metric, resolved from an algorithm entry's `metric`
/// property via the field's `vectorSearchProfile` → profile → algorithm
/// chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    DotProduct,
    Euclidean,
}

impl Metric {
    /// Parses the Azure `metric` spelling. Only the documented spellings are
    /// accepted; anything else is rejected with `400 InvalidIndex` by the
    /// caller. Named `parse` (rather than `from_str`) to avoid confusion
    /// with [`std::str::FromStr::from_str`].
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "cosine" => Some(Metric::Cosine),
            "dotProduct" => Some(Metric::DotProduct),
            "euclidean" => Some(Metric::Euclidean),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Metric::Cosine => "cosine",
            Metric::DotProduct => "dotProduct",
            Metric::Euclidean => "euclidean",
        }
    }
}

/// Converts an HNSW `distance` (as returned in [`hnsw_rs::prelude::Neighbour`])
/// into the emulator's `@search.score` for the given metric.
///
/// Note: there is deliberately **no** dot-product distance wrapper here.
/// `hnsw_rs` requires non-negative distances (it asserts
/// `dist_to_ref >= 0` internally), and `anndists`'s `DistDot` asserts
/// `dot <= 1` — so neither negation (`-dot`, negative for positive dots)
/// nor `1 - dot` (negative for large unnormalized dots) can back a graph.
/// `dotProduct` therefore always executes as an exact brute-force scan over
/// the raw inner product (see [`brute_force_score`]); this arm exists for
/// completeness only.
#[must_use]
pub fn score_from_distance(metric: Metric, distance: f32) -> f32 {
    match metric {
        // `DistCosine` evaluates to `1 - cos_sim`.
        Metric::Cosine => 1.0 - distance,
        // Unreachable via the graph (dotProduct always scans); kept so the
        // mapping is total.
        Metric::DotProduct => -distance,
        // `DistL2` evaluates to the raw L2 distance.
        Metric::Euclidean => 1.0 / (1.0 + distance),
    }
}

/// Exact similarity score for the brute-force path, computed directly from
/// the raw vectors.
#[must_use]
pub fn brute_force_score(metric: Metric, query: &[f32], stored: &[f32]) -> f32 {
    debug_assert_eq!(query.len(), stored.len());
    match metric {
        Metric::Cosine => cosine_similarity(query, stored),
        Metric::DotProduct => dot(query, stored),
        Metric::Euclidean => 1.0 / (1.0 + l2_distance(query, stored)),
    }
}

/// Raw inner product.
#[must_use]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Euclidean (L2) distance.
#[must_use]
pub fn l2_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

/// Cosine similarity in `[-1, 1]` (`0` when either vector is all zeros, where
/// the angle is undefined). Matches `1 - DistCosine` on non-degenerate
/// inputs; `DistCosine` uses `f64` accumulation internally, so tiny
/// last-ulp differences are possible.
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let dot_f64: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let norm_a: f64 = a
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    let norm_b: f64 = b
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        // Clamp for float rounding so the score stays in [-1, 1].
        #[allow(clippy::cast_possible_truncation)]
        let similarity = (dot_f64 / (norm_a * norm_b)) as f32;
        similarity.clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hnsw_rs::prelude::Distance as _;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn metric_round_trips() {
        for (raw, metric) in [
            ("cosine", Metric::Cosine),
            ("dotProduct", Metric::DotProduct),
            ("euclidean", Metric::Euclidean),
        ] {
            assert_eq!(Metric::parse(raw), Some(metric));
            assert_eq!(metric.as_str(), raw);
        }
        assert_eq!(Metric::parse("cosineSimilarity"), None);
        assert_eq!(Metric::parse(""), None);
    }

    #[test]
    fn dot_product_is_exact_for_negative_and_large_magnitudes() {
        // The brute-force dot path (the only dotProduct execution path)
        // supports negative dots and large unnormalized magnitudes that no
        // non-negative HNSW distance wrapper can represent.
        let a = vec![1.0_f32, 2.0, 3.0];
        let far = vec![-1.0_f32, 0.0, 0.0];
        assert!(approx_eq(
            brute_force_score(Metric::DotProduct, &a, &far),
            -1.0
        ));
        let big = vec![100.0_f32; 8];
        assert!(approx_eq(
            brute_force_score(Metric::DotProduct, &big, &big),
            8.0 * 100.0 * 100.0
        ));
    }

    #[test]
    fn score_from_distance_matches_brute_force() {
        let a = vec![0.5_f32, -1.25, 3.0];
        let b = vec![2.0_f32, 0.5, -1.0];
        let cosine = hnsw_rs::prelude::DistCosine.eval(&a, &b);
        assert!(approx_eq(
            score_from_distance(Metric::Cosine, cosine),
            brute_force_score(Metric::Cosine, &a, &b)
        ));
        let l2 = hnsw_rs::prelude::DistL2.eval(&a, &b);
        assert!(approx_eq(
            score_from_distance(Metric::Euclidean, l2),
            brute_force_score(Metric::Euclidean, &a, &b)
        ));
    }

    #[test]
    fn cosine_handles_unnormalized_and_zero_vectors() {
        // Same direction, different magnitudes → similarity 1.
        assert!(approx_eq(cosine_similarity(&[3.0, 0.0], &[7.0, 0.0]), 1.0));
        // Orthogonal → 0.
        assert!(approx_eq(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0));
        // Opposite → -1.
        assert!(approx_eq(
            cosine_similarity(&[1.0, 0.0], &[-2.0, 0.0]),
            -1.0
        ));
        // Zero vector → 0 (angle undefined; exact early-return value).
        assert!(approx_eq(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), 0.0));
    }
}
