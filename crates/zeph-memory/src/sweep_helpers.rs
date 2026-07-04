// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared helpers for the background sweep loops in [`crate::scenes`] and [`crate::tiers`].
//!
//! Both sweeps cluster candidates by cosine similarity, batch-embed with the same
//! validation rules, and call the LLM under the same fixed timeout. Extracted here to
//! avoid duplicating that logic across the two modules (#5581).

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message};

use crate::error::MemoryError;
use zeph_common::math::cosine_similarity;

/// Fixed timeout applied to every LLM call issued from a sweep loop.
const LLM_SWEEP_TIMEOUT_SECS: u64 = 15;

/// Cluster `items` by cosine similarity using greedy nearest-neighbor.
///
/// Each item is compared to the representative (first member) of existing clusters; if
/// similarity to that representative is `>= threshold`, it joins that cluster, otherwise
/// it starts a new one. Items with an empty embedding vector always start their own
/// singleton cluster (their similarity to anything is defined as `0.0`, so this is
/// equivalent to falling through every existing cluster, made explicit here for clarity
/// and to skip the unnecessary inner-loop comparisons).
///
/// This is O(n * k) where k is the number of clusters formed, not O(n^2).
pub(crate) fn cluster_by_cosine_similarity<T>(
    items: Vec<(T, Vec<f32>)>,
    threshold: f32,
) -> Vec<Vec<(T, Vec<f32>)>> {
    let mut clusters: Vec<Vec<(T, Vec<f32>)>> = Vec::new();

    'outer: for item in items {
        if item.1.is_empty() {
            clusters.push(vec![item]);
            continue;
        }

        for cluster in &mut clusters {
            let rep = &cluster[0].1;
            if rep.is_empty() {
                continue;
            }
            if cosine_similarity(&item.1, rep) >= threshold {
                cluster.push(item);
                continue 'outer;
            }
        }

        clusters.push(vec![item]);
    }

    clusters
}

/// Outcome of a validated batch-embed call.
pub(crate) enum EmbedBatchOutcome {
    /// Embeddings were returned and match `texts` in length, one-to-one.
    Ok(Vec<Vec<f32>>),
    /// The call failed or returned a mismatched number of vectors; already logged.
    Skip,
}

/// Batch-embed `texts`, validating the result length before handing it back to the caller.
///
/// Assumes the caller has already confirmed `provider.supports_embeddings()` — that gate,
/// and any no-embedding-support fallback, stays at the call site because the two current
/// callers (`scenes`, `tiers`) diverge on that path.
///
/// The tracing span around the `embed_batch` call is deliberately built by the caller
/// (via `.instrument()`), not here: `tracing::info_span!` requires its name to be a
/// literal known at compile time, so it cannot be threaded through as a runtime
/// parameter. Each caller keeps its own literal span name (`memory.scenes.embed_batch` /
/// `memory.tiers.embed_batch`).
pub(crate) async fn embed_batch_with_validation(
    provider: &AnyProvider,
    texts: &[&str],
    log_tag: &'static str,
) -> EmbedBatchOutcome {
    let vecs = provider.embed_batch(texts).await;
    match vecs {
        Ok(vecs) => {
            if vecs.len() != texts.len() {
                tracing::warn!(
                    expected = texts.len(),
                    got = vecs.len(),
                    "{log_tag}: embed_batch length mismatch, skipping sweep"
                );
                return EmbedBatchOutcome::Skip;
            }
            EmbedBatchOutcome::Ok(vecs)
        }
        Err(e) => {
            tracing::warn!(error = %e, "{log_tag}: batch embed failed, skipping sweep");
            EmbedBatchOutcome::Skip
        }
    }
}

/// Call the LLM with a fixed sweep timeout, mapping timeout and provider errors uniformly.
///
/// Callers build their own prompt `messages` and parse the returned text themselves.
pub(crate) async fn llm_call_with_timeout(
    provider: &AnyProvider,
    messages: &[Message],
    timeout_label: &str,
) -> Result<String, MemoryError> {
    tokio::time::timeout(
        Duration::from_secs(LLM_SWEEP_TIMEOUT_SECS),
        provider.chat(messages),
    )
    .await
    .map_err(|_| MemoryError::Timeout(format!("{timeout_label} timed out after 15s")))?
    .map_err(MemoryError::Llm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageId;

    #[test]
    fn cluster_by_cosine_similarity_groups_similar() {
        let v1 = vec![1.0f32, 0.0, 0.0];
        let v2 = vec![1.0f32, 0.0, 0.0];
        let v3 = vec![0.0f32, 1.0, 0.0];

        let items = vec![(MessageId(1), v1), (MessageId(2), v2), (MessageId(3), v3)];

        let clusters = cluster_by_cosine_similarity(items, 0.80);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].len(), 2);
        assert_eq!(clusters[1].len(), 1);
    }

    #[test]
    fn cluster_by_cosine_similarity_groups_identical() {
        let v1 = vec![1.0f32, 0.0, 0.0];
        let v2 = vec![1.0f32, 0.0, 0.0];
        let v3 = vec![0.0f32, 1.0, 0.0]; // orthogonal

        let items = vec![(MessageId(1), v1), (MessageId(2), v2), (MessageId(3), v3)];

        let clusters = cluster_by_cosine_similarity(items, 0.92f32);
        assert_eq!(clusters.len(), 2, "should produce 2 clusters");
        assert_eq!(clusters[0].len(), 2, "first cluster should have 2 members");
        assert_eq!(clusters[1].len(), 1, "second cluster is the orthogonal one");
    }

    #[test]
    fn cluster_by_cosine_similarity_empty_embeddings_become_singletons() {
        let items = vec![(MessageId(1), vec![]), (MessageId(2), vec![])];
        let clusters = cluster_by_cosine_similarity(items, 0.92);
        assert_eq!(clusters.len(), 2);
    }

    #[tokio::test]
    async fn embed_batch_with_validation_success_returns_ok() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let provider =
            AnyProvider::Mock(MockProvider::default().with_embedding(vec![1.0, 0.0, 0.0]));
        let texts = ["a", "b"];

        match embed_batch_with_validation(&provider, &texts, "test").await {
            EmbedBatchOutcome::Ok(vecs) => assert_eq!(vecs.len(), 2),
            EmbedBatchOutcome::Skip => panic!("expected Ok, got Skip"),
        }
    }

    #[tokio::test]
    async fn embed_batch_with_validation_provider_error_returns_skip() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let provider = AnyProvider::Mock(
            MockProvider::default()
                .with_embedding(vec![1.0, 0.0, 0.0])
                .with_embed_invalid_input(),
        );
        let texts = ["a", "b"];

        match embed_batch_with_validation(&provider, &texts, "test").await {
            EmbedBatchOutcome::Ok(_) => panic!("expected Skip, got Ok"),
            EmbedBatchOutcome::Skip => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn llm_call_with_timeout_times_out() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{MessageMetadata, Role};

        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec!["late reply".into()])
                .with_delay(LLM_SWEEP_TIMEOUT_SECS * 1_000 + 1_000),
        );
        let messages = vec![Message {
            role: Role::User,
            content: "hello".to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = llm_call_with_timeout(&provider, &messages, "test call").await;
        assert!(
            matches!(result, Err(MemoryError::Timeout(_))),
            "expected Timeout error, got {result:?}"
        );
    }
}
