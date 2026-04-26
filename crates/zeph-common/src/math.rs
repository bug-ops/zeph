// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Mathematical utilities for vector operations.
//!
//! This module provides general-purpose vector math as well as the
//! [`EmbeddingVector<State>`] typestate wrapper that encodes L2-normalization at the
//! type level. Use [`EmbeddingVector::<Normalized>`] as the required parameter type on
//! functions that feed vectors directly into Qdrant cosine-distance searches.

use std::marker::PhantomData;

// ── Typestate markers ────────────────────────────────────────────────────────

/// Typestate marker indicating that an [`EmbeddingVector`] has been L2-normalized
/// to unit length.
///
/// This marker cannot be constructed outside this module — it can only be created
/// by [`EmbeddingVector::normalize`] or the trust-caller constructor
/// [`EmbeddingVector::<Normalized>::new_normalized`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Normalized(());

/// Typestate marker indicating that an [`EmbeddingVector`] has **not** been
/// normalized yet (raw output from a model or loaded from storage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unnormalized(());

// ── EmbeddingVector ──────────────────────────────────────────────────────────

/// An embedding vector tagged with a normalization-state marker.
///
/// The type parameter encodes whether the vector is L2-normalized:
///
/// - `EmbeddingVector<Unnormalized>` — raw model output; must be normalized before
///   passing to cosine-distance Qdrant searches.
/// - `EmbeddingVector<Normalized>` — unit-length vector, safe to pass directly to
///   Qdrant gRPC cosine queries.
///
/// Using [`Normalized`] as a required parameter type at the Qdrant search boundary
/// turns dimension/normalization mismatches into compile-time errors rather than
/// silent near-zero similarity scores (see bugs #3421, #3382, #3420, #3422).
///
/// # Construction
///
/// ```
/// use zeph_common::math::{EmbeddingVector, Normalized, Unnormalized};
///
/// // Wrap a raw model vector and normalize it.
/// let raw = EmbeddingVector::<Unnormalized>::new(vec![3.0_f32, 4.0]);
/// let normalized = raw.normalize();
/// let slice = normalized.as_slice();
/// // A normalized vector has unit L2 length.
/// let norm: f32 = slice.iter().map(|x| x * x).sum::<f32>().sqrt();
/// assert!((norm - 1.0).abs() < 1e-6);
///
/// // Trust-caller constructor for models that always return unit vectors.
/// let trusted = EmbeddingVector::<Normalized>::new_normalized(vec![0.6_f32, 0.8]);
/// assert_eq!(trusted.as_slice(), &[0.6_f32, 0.8]);
/// ```
#[derive(Debug, Clone)]
pub struct EmbeddingVector<State> {
    inner: Vec<f32>,
    _state: PhantomData<State>,
}

impl EmbeddingVector<Unnormalized> {
    /// Wrap a raw embedding vector from a model or storage.
    ///
    /// The returned vector is tagged `Unnormalized`. Call [`normalize`](Self::normalize)
    /// before passing it to functions that require [`Normalized`].
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::math::{EmbeddingVector, Unnormalized};
    ///
    /// let v = EmbeddingVector::<Unnormalized>::new(vec![1.0_f32, 0.0]);
    /// assert_eq!(v.as_slice(), &[1.0_f32, 0.0]);
    /// ```
    #[must_use]
    pub fn new(inner: Vec<f32>) -> Self {
        Self {
            inner,
            _state: PhantomData,
        }
    }

    /// L2-normalize this vector and return an [`EmbeddingVector<Normalized>`].
    ///
    /// If the vector is a zero vector (L2 norm is zero), all elements are set to zero
    /// to avoid division by zero; the result is technically invalid for cosine search
    /// but is safe and consistent with the behavior of [`cosine_similarity`].
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::math::{EmbeddingVector, Unnormalized};
    ///
    /// let raw = EmbeddingVector::<Unnormalized>::new(vec![3.0_f32, 4.0]);
    /// let norm = raw.normalize();
    /// let sum_sq: f32 = norm.as_slice().iter().map(|x| x * x).sum();
    /// assert!((sum_sq - 1.0).abs() < 1e-6, "must be unit length");
    /// ```
    #[must_use]
    pub fn normalize(self) -> EmbeddingVector<Normalized> {
        let norm: f32 = self.inner.iter().map(|x| x * x).sum::<f32>().sqrt();
        let normalized = if norm < f32::EPSILON {
            self.inner
        } else {
            self.inner.into_iter().map(|x| x / norm).collect()
        };
        EmbeddingVector {
            inner: normalized,
            _state: PhantomData,
        }
    }
}

impl EmbeddingVector<Normalized> {
    /// Construct a normalized embedding vector, trusting the caller that `inner` is
    /// already L2-unit-length.
    ///
    /// Use this constructor only when the source guarantees unit-length output (e.g., a
    /// model that always normalizes, or a vector loaded from a store known to hold
    /// normalized data). Incorrect use does **not** cause UB but will produce wrong
    /// cosine scores.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::math::{EmbeddingVector, Normalized};
    ///
    /// // Suppose the model always returns unit vectors.
    /// let v = EmbeddingVector::<Normalized>::new_normalized(vec![0.6_f32, 0.8]);
    /// assert_eq!(v.as_slice(), &[0.6_f32, 0.8]);
    /// ```
    #[must_use]
    pub fn new_normalized(inner: Vec<f32>) -> Self {
        Self {
            inner,
            _state: PhantomData,
        }
    }
}

impl<State> EmbeddingVector<State> {
    /// Return a borrowed slice of the vector elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::math::{EmbeddingVector, Unnormalized};
    ///
    /// let v = EmbeddingVector::<Unnormalized>::new(vec![1.0_f32, 2.0]);
    /// assert_eq!(v.as_slice(), &[1.0_f32, 2.0]);
    /// ```
    #[must_use]
    pub fn as_slice(&self) -> &[f32] {
        &self.inner
    }

    /// Consume the wrapper and return the underlying `Vec<f32>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::math::{EmbeddingVector, Unnormalized};
    ///
    /// let v = EmbeddingVector::<Unnormalized>::new(vec![1.0_f32, 2.0]);
    /// assert_eq!(v.into_inner(), vec![1.0_f32, 2.0]);
    /// ```
    #[must_use]
    pub fn into_inner(self) -> Vec<f32> {
        self.inner
    }

    /// Return the number of dimensions in this vector.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::math::{EmbeddingVector, Unnormalized};
    ///
    /// let v = EmbeddingVector::<Unnormalized>::new(vec![1.0_f32, 2.0, 3.0]);
    /// assert_eq!(v.len(), 3);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the vector has no elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::math::{EmbeddingVector, Unnormalized};
    ///
    /// let v = EmbeddingVector::<Unnormalized>::new(vec![]);
    /// assert!(v.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl From<Vec<f32>> for EmbeddingVector<Unnormalized> {
    fn from(v: Vec<f32>) -> Self {
        Self::new(v)
    }
}

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
    if denom < f32::EPSILON {
        return 0.0;
    }

    (dot / denom).clamp(-1.0, 1.0)
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
        assert!(cosine_similarity(&a, &b).abs() <= f32::EPSILON);
    }

    #[test]
    fn different_lengths() {
        let a = vec![1.0_f32];
        let b = vec![1.0_f32, 0.0];
        assert!(cosine_similarity(&a, &b).abs() <= f32::EPSILON);
    }

    #[test]
    fn empty_vectors() {
        assert!(cosine_similarity(&[], &[]).abs() <= f32::EPSILON);
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

    // ── EmbeddingVector tests ────────────────────────────────────────────────

    #[test]
    fn embedding_vector_normalize_produces_unit_vector() {
        let raw = EmbeddingVector::<Unnormalized>::new(vec![3.0_f32, 4.0]);
        let normed = raw.normalize();
        let sum_sq: f32 = normed.as_slice().iter().map(|x| x * x).sum();
        assert!((sum_sq - 1.0).abs() < 1e-6);
    }

    #[test]
    fn embedding_vector_normalize_zero_vector_is_safe() {
        let raw = EmbeddingVector::<Unnormalized>::new(vec![0.0_f32, 0.0]);
        let normed = raw.normalize();
        assert_eq!(normed.as_slice(), &[0.0_f32, 0.0]);
    }

    #[test]
    fn embedding_vector_into_inner_roundtrip() {
        let data = vec![1.0_f32, 2.0, 3.0];
        let v = EmbeddingVector::<Unnormalized>::new(data.clone());
        assert_eq!(v.into_inner(), data);
    }

    #[test]
    fn embedding_vector_len_and_is_empty() {
        let v = EmbeddingVector::<Unnormalized>::new(vec![1.0_f32, 2.0]);
        assert_eq!(v.len(), 2);
        assert!(!v.is_empty());

        let empty = EmbeddingVector::<Unnormalized>::new(vec![]);
        assert!(empty.is_empty());
    }

    #[test]
    fn embedding_vector_new_normalized_trust_caller() {
        let v = EmbeddingVector::<Normalized>::new_normalized(vec![0.6_f32, 0.8]);
        assert_eq!(v.as_slice(), &[0.6_f32, 0.8]);
    }

    #[test]
    fn embedding_vector_from_vec() {
        let v: EmbeddingVector<Unnormalized> = vec![1.0_f32, 2.0].into();
        assert_eq!(v.as_slice(), &[1.0_f32, 2.0]);
    }
}
