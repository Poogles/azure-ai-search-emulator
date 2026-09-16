//! Reranker score normalization: maps raw engine scores to a 0.0–1.0
//! range using a sigmoid transform.

/// Normalizes a raw BM25 score to a 0.0–1.0 range using the transform
/// `score / (score + 1)`. BM25 scores are unbounded above, so this maps
/// them into a bounded range while preserving ordering.
#[must_use]
pub fn normalize_bm25(score: f32) -> f32 {
    if score <= 0.0 {
        return 0.0;
    }
    score / (score + 1.0)
}

/// Normalizes a raw cosine similarity score to a 0.0–1.0 range. Cosine
/// similarity is already in [-1.0, 1.0]; we clamp to [0.0, 1.0] (negative
/// similarity means the document is irrelevant, so it maps to 0.0).
#[must_use]
pub fn normalize_cosine(score: f32) -> f32 {
    score.clamp(0.0, 1.0)
}

/// Normalizes a raw score to a 0.0–1.0 range, selecting the transform
/// based on the score source.
///
/// * `is_vector` — whether the score came from a vector (cosine) search.
#[must_use]
pub fn normalize(score: f32, is_vector: bool) -> f32 {
    if is_vector {
        normalize_cosine(score)
    } else {
        normalize_bm25(score)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn bm25_zero() {
        assert_eq!(normalize_bm25(0.0), 0.0);
    }

    #[test]
    fn bm25_negative() {
        assert_eq!(normalize_bm25(-1.0), 0.0);
    }

    #[test]
    fn bm25_positive() {
        let result = normalize_bm25(1.0);
        assert!((result - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn bm25_large() {
        let result = normalize_bm25(1000.0);
        assert!(result < 1.0);
        assert!(result > 0.99);
    }

    #[test]
    fn cosine_negative() {
        assert_eq!(normalize_cosine(-0.5), 0.0);
    }

    #[test]
    fn cosine_zero() {
        assert_eq!(normalize_cosine(0.0), 0.0);
    }

    #[test]
    fn cosine_one() {
        assert_eq!(normalize_cosine(1.0), 1.0);
    }

    #[test]
    fn cosine_above_one() {
        assert_eq!(normalize_cosine(1.5), 1.0);
    }

    #[test]
    fn normalize_dispatches_correctly() {
        assert!((normalize(1.0, false) - 0.5).abs() < f32::EPSILON);
        assert_eq!(normalize(1.0, true), 1.0);
    }
}
