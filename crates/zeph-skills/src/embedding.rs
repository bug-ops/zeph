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
    /// The vector is L2-normalized to unit length before storage. This
    /// matches the vectors Qdrant returns for `Distance::Cosine` collections,
    /// so `RoutingHeadInner::score` (`rl_head.rs`) sees the same input scale
    /// regardless of which `vector_backend` produced the embedding. Cosine
    /// similarity is scale-invariant, so normalizing here doesn't change any
    /// existing similarity/ranking result — see `zeph_common::math::normalize`.
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
        let normalized =
            zeph_common::math::EmbeddingVector::<zeph_common::math::Unnormalized>::new(vec)
                .normalize()
                .into_inner();
        Self(normalized)
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
    /// // `from_raw` normalizes, so the returned vector is unit-length, not `v` itself.
    /// let v = vec![3.0_f32, 4.0];
    /// let emb = SkillEmbedding::from_raw(v);
    /// assert_eq!(emb.into_inner(), vec![0.6_f32, 0.8]);
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
/// Tries [`LlmProvider::embed_batch`] first, wrapped in a single `timeout` for the whole
/// batch — one round trip on the common success path instead of `skills.len()` sequential
/// calls. If the batch call errors, times out, or returns a mismatched result count, this
/// falls back to embedding each skill individually (same per-item `timeout`), so one bad
/// skill doesn't lose the others. Skills that fail or time out in the fallback path are
/// skipped with a `tracing::warn!` and are not included in the returned vector.
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
    if skills.is_empty() {
        return Vec::new();
    }

    let texts: Vec<&str> = skills.iter().map(|s| s.description.as_str()).collect();
    match tokio::time::timeout(timeout, embed_provider.embed_batch(&texts)).await {
        Ok(Ok(embeddings)) if embeddings.len() == skills.len() => {
            return skills
                .iter()
                .cloned()
                .zip(embeddings)
                .map(|(skill, emb)| (skill, SkillEmbedding::from_raw(emb)))
                .collect();
        }
        Ok(Ok(embeddings)) => {
            tracing::warn!(
                expected = skills.len(),
                actual = embeddings.len(),
                "embed_batch returned mismatched result count, falling back to per-item embedding"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "embed_batch failed, falling back to per-item embedding");
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "embed_batch timed out, falling back to per-item embedding"
            );
        }
    }

    embed_skills_sequential(skills, embed_provider, timeout).await
}

/// Fallback path: embed each skill independently with a per-call `timeout`.
///
/// Used by [`embed_skills_with_timeout`] when the batched call fails, so one bad or slow
/// skill doesn't cause the whole batch to be lost.
async fn embed_skills_sequential(
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
    use std::assert_matches;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn new_valid_dimension() {
        let emb = SkillEmbedding::new(vec![1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(emb.dim(), 3);
    }

    #[test]
    fn new_dimension_mismatch() {
        let err = SkillEmbedding::new(vec![1.0, 0.0], 3).unwrap_err();
        assert_matches!(
            err,
            SkillError::EmbeddingDimMismatch {
                expected: 3,
                actual: 2
            }
        );
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
    fn as_ref_returns_normalized_slice() {
        let emb = SkillEmbedding::from_raw(vec![3.0_f32, 4.0]);
        assert_eq!(emb.as_ref(), &[0.6_f32, 0.8]);
    }

    #[test]
    fn into_inner_returns_normalized_vec() {
        let emb = SkillEmbedding::from_raw(vec![3.0_f32, 4.0]);
        assert_eq!(emb.into_inner(), vec![0.6_f32, 0.8]);
    }

    #[test]
    fn clone_preserves_data() {
        let emb = SkillEmbedding::from_raw(vec![1.0, 2.0]);
        assert_eq!(emb.clone(), emb);
    }

    #[test]
    fn from_raw_normalizes_to_unit_length() {
        let emb = SkillEmbedding::from_raw(vec![1.0_f32, 2.0, 3.0]);
        let sum_sq: f32 = emb.as_ref().iter().map(|x| x * x).sum();
        assert!(
            (sum_sq - 1.0).abs() < 1e-6,
            "L2 norm must be ~1.0, got sum_sq={sum_sq}"
        );
    }

    #[test]
    fn from_raw_zero_vector_stays_zero() {
        let emb = SkillEmbedding::from_raw(vec![0.0_f32, 0.0, 0.0]);
        assert_eq!(emb.as_ref(), &[0.0_f32, 0.0, 0.0]);
        assert!(emb.as_ref().iter().all(|x| x.is_finite()));
    }

    #[test]
    fn new_empty_dimension_zero() {
        let emb = SkillEmbedding::new(vec![], 0).unwrap();
        assert_eq!(emb.dim(), 0);
    }

    #[test]
    fn from_raw_preserves_sign_of_negative_components() {
        let emb = SkillEmbedding::from_raw(vec![-3.0_f32, 4.0]);
        assert_eq!(emb.as_ref(), &[-0.6_f32, 0.8]);
    }

    #[test]
    fn from_raw_normalizes_dominant_magnitude_component() {
        // One huge-magnitude component alongside many small ones: normalization must not
        // overflow/underflow and the unit-length invariant must still hold.
        let mut raw = vec![1e6_f32];
        raw.extend(vec![1e-3_f32; 10]);
        let emb = SkillEmbedding::from_raw(raw);
        let sum_sq: f32 = emb.as_ref().iter().map(|x| x * x).sum();
        assert!(
            (sum_sq - 1.0).abs() < 1e-5,
            "L2 norm must be ~1.0 even with a dominant component, got sum_sq={sum_sq}"
        );
        assert!(emb.as_ref().iter().all(|x| x.is_finite()));
    }

    /// Test double distinguishing the batch path from the per-item fallback path: `embed_batch`
    /// returns `[0.0, 1.0]` vectors, `embed` returns `[1.0, 0.0]` vectors, so assertions on the
    /// returned embedding reveal which path actually produced a given result.
    #[derive(Default)]
    struct BatchTestProvider {
        embed_calls: AtomicUsize,
        batch_calls: AtomicUsize,
        batch_delay: Duration,
        batch_fail: bool,
        batch_short: bool,
        /// 1-indexed `embed()` call number that should fail; `None` means every call succeeds.
        /// Lets a test pin down exactly *which* skill fails inside the per-item fallback loop,
        /// to prove partial-failure tolerance (not just all-succeed or all-fail).
        fail_embed_at_call: Option<usize>,
    }

    impl LlmProvider for BatchTestProvider {
        async fn chat(
            &self,
            _messages: &[zeph_llm::provider::Message],
        ) -> Result<String, zeph_llm::LlmError> {
            unimplemented!("not exercised by embed_skills_with_timeout")
        }

        async fn chat_stream(
            &self,
            _messages: &[zeph_llm::provider::Message],
        ) -> Result<zeph_llm::provider::ChatStream, zeph_llm::LlmError> {
            unimplemented!("not exercised by embed_skills_with_timeout")
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, zeph_llm::LlmError> {
            let call_no = self.embed_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_embed_at_call == Some(call_no) {
                return Err(zeph_llm::LlmError::Unavailable);
            }
            Ok(vec![1.0, 0.0])
        }

        fn embed_batch(
            &self,
            texts: &[&str],
        ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>, zeph_llm::LlmError>> + Send
        {
            self.batch_calls.fetch_add(1, Ordering::SeqCst);
            let requested = texts.len();
            let fail = self.batch_fail;
            let short = self.batch_short;
            let delay = self.batch_delay;
            async move {
                if delay > Duration::ZERO {
                    tokio::time::sleep(delay).await;
                }
                if fail {
                    return Err(zeph_llm::LlmError::Unavailable);
                }
                let returned = if short {
                    requested.saturating_sub(1)
                } else {
                    requested
                };
                Ok(vec![vec![0.0, 1.0]; returned])
            }
        }

        fn supports_embeddings(&self) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "batch-test"
        }
    }

    fn make_test_skill(name: &str) -> SkillMeta {
        let content = format!(
            "---\nname: {name}\ndescription: A test skill.\n---\n\n## Usage\n\nDo stuff.\n"
        );
        crate::loader::load_skill_meta_from_str(&content).unwrap().0
    }

    #[tokio::test]
    async fn embed_skills_with_timeout_empty_skips_provider_call() {
        let provider = BatchTestProvider::default();
        let result = embed_skills_with_timeout(&[], &provider, Duration::from_secs(1)).await;
        assert!(result.is_empty());
        assert_eq!(provider.batch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.embed_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn embed_skills_with_timeout_uses_batch_on_success() {
        let provider = BatchTestProvider::default();
        let skills = vec![
            make_test_skill("a"),
            make_test_skill("b"),
            make_test_skill("c"),
        ];
        let result = embed_skills_with_timeout(&skills, &provider, Duration::from_secs(1)).await;

        assert_eq!(result.len(), 3);
        for (_, emb) in &result {
            assert_eq!(
                emb.as_ref(),
                &[0.0_f32, 1.0],
                "expected batch-path marker vector"
            );
        }
        assert_eq!(provider.batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider.embed_calls.load(Ordering::SeqCst),
            0,
            "successful batch call must not fall back to per-item embedding"
        );
    }

    #[tokio::test]
    async fn embed_skills_with_timeout_falls_back_on_batch_error() {
        let provider = BatchTestProvider {
            batch_fail: true,
            ..Default::default()
        };
        let skills = vec![
            make_test_skill("a"),
            make_test_skill("b"),
            make_test_skill("c"),
        ];
        let result = embed_skills_with_timeout(&skills, &provider, Duration::from_secs(1)).await;

        assert_eq!(result.len(), 3, "fallback must recover all skills");
        for (_, emb) in &result {
            assert_eq!(
                emb.as_ref(),
                &[1.0_f32, 0.0],
                "expected per-item fallback marker vector"
            );
        }
        assert_eq!(provider.batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.embed_calls.load(Ordering::SeqCst), 3);
    }

    /// Regression test for the exact guarantee #6481 asked to preserve: a batch failure
    /// falling back to the per-item loop must still tolerate *individual* skill failures
    /// inside that fallback — one bad skill must not lose the others, and it must not silently
    /// let every skill through either. The prior three fallback tests only proved
    /// batch → fallback routing with a uniformly-succeeding fallback; this proves the
    /// fallback's own skip-and-continue behavior survives the new call path.
    #[tokio::test]
    async fn embed_skills_with_timeout_fallback_tolerates_individual_skill_failure() {
        let provider = BatchTestProvider {
            batch_fail: true,
            fail_embed_at_call: Some(2),
            ..Default::default()
        };
        let skills = vec![
            make_test_skill("a"),
            make_test_skill("b"),
            make_test_skill("c"),
        ];
        let result = embed_skills_with_timeout(&skills, &provider, Duration::from_secs(1)).await;

        assert_eq!(
            result.len(),
            2,
            "exactly one skill's embed call fails; result must be neither 0 nor 3"
        );
        let names: Vec<&str> = result
            .iter()
            .map(|(skill, _)| skill.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["a", "c"],
            "skill b's embed call failed and must be skipped; a and c must survive"
        );
        for (_, emb) in &result {
            assert_eq!(
                emb.as_ref(),
                &[1.0_f32, 0.0],
                "expected per-item fallback marker vector"
            );
        }
        assert_eq!(provider.batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider.embed_calls.load(Ordering::SeqCst),
            3,
            "fallback must still attempt every skill even after one fails"
        );
    }

    #[tokio::test]
    async fn embed_skills_with_timeout_falls_back_on_mismatched_batch_length() {
        let provider = BatchTestProvider {
            batch_short: true,
            ..Default::default()
        };
        let skills = vec![
            make_test_skill("a"),
            make_test_skill("b"),
            make_test_skill("c"),
        ];
        let result = embed_skills_with_timeout(&skills, &provider, Duration::from_secs(1)).await;

        assert_eq!(result.len(), 3, "fallback must recover all skills");
        for (_, emb) in &result {
            assert_eq!(
                emb.as_ref(),
                &[1.0_f32, 0.0],
                "expected per-item fallback marker vector"
            );
        }
        assert_eq!(provider.batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.embed_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn embed_skills_with_timeout_falls_back_on_batch_timeout() {
        // batch_delay comfortably exceeds the configured timeout so the batch call is
        // guaranteed to time out; the per-item fallback (no artificial delay) then succeeds.
        let provider = BatchTestProvider {
            batch_delay: Duration::from_millis(300),
            ..Default::default()
        };
        let skills = vec![make_test_skill("a"), make_test_skill("b")];
        let result = embed_skills_with_timeout(&skills, &provider, Duration::from_millis(50)).await;

        assert_eq!(result.len(), 2, "fallback must recover all skills");
        for (_, emb) in &result {
            assert_eq!(
                emb.as_ref(),
                &[1.0_f32, 0.0],
                "expected per-item fallback marker vector"
            );
        }
        assert_eq!(provider.batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.embed_calls.load(Ordering::SeqCst), 2);
    }
}
