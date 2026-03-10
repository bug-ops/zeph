// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Mathematical utilities for vector operations.

/// Compute cosine similarity between two equal-length f32 vectors.
///
/// Returns `0.0` if the vectors have different lengths, are empty, or if
/// either vector is a zero vector.
///
/// Uses a single-pass loop for efficiency.
#[inline]
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    debug_assert_eq!(a.len(), b.len(), "cosine_similarity: length mismatch");

    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        return 0.0;
    }

    dot / denom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_vectors() {
        let v = vec![1.0_f32, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn opposite_vectors() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![-1.0_f32, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn zero_vector() {
        let a = vec![0.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn different_lengths() {
        let a = vec![1.0_f32];
        let b = vec![1.0_f32, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn empty_vectors() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn parallel_vectors() {
        let a = vec![2.0_f32, 0.0];
        let b = vec![5.0_f32, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalized_vectors() {
        let s = 1.0_f32 / 2.0_f32.sqrt();
        let a = vec![s, s];
        let b = vec![s, s];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }
}
