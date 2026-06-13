// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validated embedding vector newtype for skill matching.
//!
//! [`SkillEmbedding`] wraps a `Vec<f32>` and enforces dimension consistency at
//! construction time, eliminating silent zero-similarity results from
//! `cosine_similarity` when vectors of different lengths are compared.
//!
//! # Examples
//!
//! ```
//! use zeph_skills::embedding::SkillEmbedding;
//!
//! // Validated construction — dimension is checked once at the boundary.
//! let emb = SkillEmbedding::new(vec![1.0, 0.0, 0.0], 3).unwrap();
//! assert_eq!(emb.dim(), 3);
//!
//! // AsRef<[f32]> keeps cosine_similarity call sites unchanged.
//! let slice: &[f32] = emb.as_ref();
//! assert_eq!(slice.len(), 3);
//!
//! // Mismatch is caught at construction.
//! assert!(SkillEmbedding::new(vec![1.0, 0.0], 3).is_err());
//! ```

use std::time::Duration;

use zeph_llm::provider::LlmProvider;

use crate::error::SkillError;
use crate::loader::SkillMeta;

/// Validated embedding vector for skill matching.
///
/// Wraps a `Vec<f32>` with a dimension guarantee: the length is checked at
/// construction time and cannot change afterwards. This prevents silent
/// cosine-similarity bugs where mismatched dimensions return `0.0`.
///
/// Use [`SkillEmbedding::new`] when an expected dimension is known (e.g. when
/// storing a centroid alongside other embeddings). Use `from_raw`
/// at the embedding-provider boundary where the dimension is whatever the model
/// returns and no cross-check is yet possible.
///
/// # Examples
///
/// ```
/// use zeph_skills::embedding::SkillEmbedding;
///
/// let emb = SkillEmbedding::new(vec![0.0, 1.0], 2).unwrap();
/// assert_eq!(emb.dim(), 2);
/// assert_eq!(emb.as_ref(), &[0.0_f32, 1.0]);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEmbedding(Vec<f32>);

impl SkillEmbedding {
    /// Create a validated embedding vector.
    ///
    /// Checks that `vec.len() == expected_dim`. Use this when you know what
    /// dimension all embeddings in a collection must share (e.g. centroid
    /// construction, deduplication).
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::EmbeddingDimMismatch`] if `vec.len() != expected_dim`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_skills::embedding::SkillEmbedding;
    ///
    /// let ok = SkillEmbedding::new(vec![1.0, 0.0], 2);
    /// assert!(ok.is_ok());
    ///
    /// let err = SkillEmbedding::new(vec![1.0, 0.0], 3);
    /// assert!(err.is_err());
    /// ```
    pub fn new(vec: Vec<f32>, expected_dim: usize) -> Result<Self, SkillError> {
        if vec.len() != expected_dim {
            return Err(SkillError::EmbeddingDimMismatch {
                expected: expected_dim,
                actual: vec.len(),
            });
        }
        Ok(Self(vec))
    }

    /// Create a `SkillEmbedding` without dimension validation.
    ///
    /// Use only at the embedding-provider boundary — i.e., immediately after
    /// receiving a vector from `embed_fn` or `embed_provider.embed()` within
    /// a single call chain. The caller guarantees that all embeddings wrapped
    /// with `from_raw` in the same matcher or miner session were produced by
    /// the same model, ensuring dimensional consistency throughout the
    /// collection.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_skills::embedding::SkillEmbedding;
    ///
    /// // At the provider boundary: dimension is whatever the model returns.
    /// let raw = vec![0.1_f32, 0.2, 0.3];
    /// let emb = SkillEmbedding::from_raw(raw);
    /// assert_eq!(emb.dim(), 3);
    /// ```
    #[must_use]
    pub fn from_raw(vec: Vec<f32>) -> Self {
        Self(vec)
    }

    /// The number of dimensions in this embedding.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_skills::embedding::SkillEmbedding;
    ///
    /// let emb = SkillEmbedding::from_raw(vec![0.0; 768]);
    /// assert_eq!(emb.dim(), 768);
    /// ```
    #[must_use]
    pub fn dim(&self) -> usize {
        self.0.len()
    }

    /// Consume the wrapper and return the inner vector.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_skills::embedding::SkillEmbedding;
    ///
    /// let v = vec![1.0_f32, 2.0, 3.0];
    /// let emb = SkillEmbedding::from_raw(v.clone());
    /// assert_eq!(emb.into_inner(), v);
    /// ```
    #[must_use]
    pub fn into_inner(self) -> Vec<f32> {
        self.0
    }
}

impl AsRef<[f32]> for SkillEmbedding {
    fn as_ref(&self) -> &[f32] {
        &self.0
    }
}

/// Compute embeddings for a skill slice, skipping skills that fail or time out.
///
/// Each skill description is embedded independently with a per-call `timeout`. Skills whose
/// embed call times out or returns an error are skipped with a `tracing::warn!` and are not
/// included in the returned vector. The caller receives only successfully embedded skills.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use zeph_skills::embedding::{embed_skills_with_timeout, SkillEmbedding};
/// use zeph_skills::loader::SkillMeta;
///
/// # async fn example(skills: &[SkillMeta], provider: &impl zeph_llm::provider::LlmProvider) {
/// let pairs = embed_skills_with_timeout(skills, provider, Duration::from_secs(5)).await;
/// assert!(pairs.len() <= skills.len());
/// # }
/// ```
#[tracing::instrument(
    name = "skills.embed_skills_with_timeout",
    skip_all,
    fields(skill_count = skills.len())
)]
pub async fn embed_skills_with_timeout(
    skills: &[SkillMeta],
    embed_provider: &impl LlmProvider,
    timeout: Duration,
) -> Vec<(SkillMeta, SkillEmbedding)> {
    let mut result = Vec::with_capacity(skills.len());
    for skill in skills {
        match tokio::time::timeout(timeout, embed_provider.embed(&skill.description)).await {
            Ok(Ok(emb)) => result.push((skill.clone(), SkillEmbedding::from_raw(emb))),
            Ok(Err(e)) => {
                tracing::warn!(skill = %skill.name, error = %e, "embed failed, skipping skill");
            }
            Err(_) => {
                tracing::warn!(
                    skill = %skill.name,
                    timeout_ms = timeout.as_millis(),
                    "embed timed out, skipping skill"
                );
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_valid_dimension() {
        let emb = SkillEmbedding::new(vec![1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(emb.dim(), 3);
    }

    #[test]
    fn new_dimension_mismatch() {
        let err = SkillEmbedding::new(vec![1.0, 0.0], 3).unwrap_err();
        assert!(matches!(
            err,
            SkillError::EmbeddingDimMismatch {
                expected: 3,
                actual: 2
            }
        ));
    }

    #[test]
    fn from_raw_any_dimension() {
        let emb = SkillEmbedding::from_raw(vec![]);
        assert_eq!(emb.dim(), 0);

        let emb = SkillEmbedding::from_raw(vec![1.0; 1024]);
        assert_eq!(emb.dim(), 1024);
    }

    #[test]
    fn dim_accessor() {
        assert_eq!(SkillEmbedding::from_raw(vec![0.0; 7]).dim(), 7);
    }

    #[test]
    fn as_ref_returns_slice() {
        let v = vec![1.0_f32, 2.0, 3.0];
        let emb = SkillEmbedding::from_raw(v.clone());
        assert_eq!(emb.as_ref(), v.as_slice());
    }

    #[test]
    fn into_inner_returns_vec() {
        let v = vec![0.5_f32, 1.0];
        let emb = SkillEmbedding::from_raw(v.clone());
        assert_eq!(emb.into_inner(), v);
    }

    #[test]
    fn clone_preserves_data() {
        let emb = SkillEmbedding::from_raw(vec![1.0, 2.0]);
        assert_eq!(emb.clone(), emb);
    }

    #[test]
    fn new_empty_dimension_zero() {
        let emb = SkillEmbedding::new(vec![], 0).unwrap();
        assert_eq!(emb.dim(), 0);
    }
}
