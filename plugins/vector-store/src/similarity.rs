//! Vector arithmetic, and the rule that keeps it meaningful.
//!
//! Everything stored is L2-normalized on the way in, so a cosine similarity is
//! a plain dot product and a score is directly comparable across a collection.
//!
//! # Vectors from different models are never compared
//!
//! [`cosine_similarity`] returns `None` for mismatched dimensions rather than
//! a number, and the store refuses at a higher level to mix embedding models
//! at all — see [`crate::store`]. Two embedders produce coordinates in
//! unrelated spaces: `text-embedding-3-small` and `nomic-embed-text` both emit
//! 768 floats for some configurations, and the cosine between them is a real
//! number in `[-1, 1]` that means absolutely nothing. Dimension equality is a
//! necessary check, never a sufficient one, which is why the model *identity*
//! is what the store actually enforces.

/// Scale a vector to unit length so that a dot product *is* cosine similarity.
///
/// Returns `Err` for a vector that is empty, non-finite, or all zeros — none
/// of which have a direction, and all of which would otherwise produce a
/// meaningless score instead of an error.
pub fn normalize_l2(mut vector: Vec<f32>) -> Result<Vec<f32>, String> {
    if vector.is_empty() {
        return Err("embedding is empty".to_string());
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err("embedding contains a non-finite value".to_string());
    }
    // Accumulate in f64: a 3072-dimension vector of ~1.0 components loses
    // visible precision in f32, and the resulting norm is wrong in a way that
    // shows up as scores slightly above 1.0.
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm <= 0.0 || !norm.is_finite() {
        return Err("embedding has zero magnitude and therefore no direction".to_string());
    }
    for value in &mut vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(vector)
}

/// Cosine similarity between two already-normalized vectors.
///
/// `None` means the dimensions differ, which means the vectors came from
/// different embedders and must not be compared. A caller that treats `None`
/// as `0.0` has reintroduced exactly the bug this signature exists to prevent,
/// so callers skip the candidate instead.
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f64> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let dot: f64 = left
        .iter()
        .zip(right.iter())
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum();
    // Rounding in the normalization can push an identical pair a hair past
    // 1.0. Clamping keeps a score inside the range the tool documents.
    Some(dot.clamp(-1.0, 1.0))
}

/// Bytes a vector of this many dimensions occupies in memory, as f32.
///
/// Used for the byte accounting the `stats` tool reports. It is the vector
/// payload only, never process memory, and is labelled that way everywhere it
/// surfaces.
pub fn vector_bytes(dimensions: usize) -> u64 {
    dimensions as u64 * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_produces_a_unit_vector() {
        let normalized = normalize_l2(vec![3.0, 4.0]).expect("has direction");
        let norm: f64 = normalized
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>()
            .sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "{norm}");
        assert!((normalized[0] - 0.6).abs() < 1e-6);
        assert!((normalized[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn a_vector_with_no_direction_is_an_error_not_a_score() {
        for (vector, expected) in [
            (vec![], "empty"),
            (vec![0.0, 0.0, 0.0], "zero magnitude"),
            (vec![1.0, f32::NAN], "non-finite"),
            (vec![f32::INFINITY, 1.0], "non-finite"),
        ] {
            let error = normalize_l2(vector).expect_err("must fail");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn identical_vectors_score_one_and_opposite_vectors_score_minus_one() {
        let a = normalize_l2(vec![1.0, 2.0, 3.0]).expect("has direction");
        let b = normalize_l2(vec![2.0, 4.0, 6.0]).expect("has direction");
        let opposite = normalize_l2(vec![-1.0, -2.0, -3.0]).expect("has direction");

        assert!((cosine_similarity(&a, &b).expect("same length") - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&a, &opposite).expect("same length") + 1.0).abs() < 1e-6);
    }

    #[test]
    fn orthogonal_vectors_score_zero() {
        let a = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let b = normalize_l2(vec![0.0, 1.0]).expect("has direction");
        assert!(cosine_similarity(&a, &b).expect("same length").abs() < 1e-6);
    }

    #[test]
    fn mismatched_dimensions_return_none_rather_than_a_number() {
        let short = normalize_l2(vec![1.0, 0.0]).expect("has direction");
        let long = normalize_l2(vec![1.0, 0.0, 0.0]).expect("has direction");
        assert_eq!(
            cosine_similarity(&short, &long),
            None,
            "a number here would be confident nonsense from two embedding spaces"
        );
        assert_eq!(cosine_similarity(&[], &[]), None);
    }

    #[test]
    fn a_score_never_escapes_the_documented_range() {
        // Repeated normalization of a long vector accumulates rounding; the
        // clamp is what keeps the documented range true.
        let vector = normalize_l2(vec![0.031_25_f32; 3_072]).expect("has direction");
        let score = cosine_similarity(&vector, &vector).expect("same length");
        assert!((-1.0..=1.0).contains(&score), "{score}");
        assert!((score - 1.0).abs() < 1e-6, "{score}");
    }

    #[test]
    fn vector_bytes_counts_four_bytes_per_dimension() {
        assert_eq!(vector_bytes(768), 3_072);
        assert_eq!(vector_bytes(0), 0);
    }
}
