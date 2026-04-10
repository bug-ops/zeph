// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AOI three-layer memory tier promotion.
//!
//! Provides a background sweep loop that promotes frequently-accessed episodic messages
//! to the semantic tier by:
//! 1. Finding candidates with `session_count >= promotion_min_sessions`.
//! 2. Grouping near-duplicate candidates by cosine similarity (greedy nearest-neighbor).
//! 3. For each cluster with >= 2 messages, calling the LLM to distill a merged fact.
//! 4. Validating the merge output (non-empty, similarity >= 0.7 to at least one original).
//! 5. Inserting the semantic fact and soft-deleting the originals.
//!
//! The sweep respects a `CancellationToken` for graceful shutdown, following the
//! same pattern as `eviction.rs`.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::store::messages::PromotionCandidate;
use crate::types::ConversationId;
use zeph_common::math::cosine_similarity;

/// Minimum cosine similarity between the merged result and at least one original for the
/// merge to be accepted. Prevents the LLM from producing semantically unrelated output.
const MERGE_VALIDATION_MIN_SIMILARITY: f32 = 0.7;

/// Configuration for the tier promotion sweep, passed from `zeph-config::TierPromotionConfig`.
///
/// Defined locally to avoid a direct dependency from `zeph-memory` on `zeph-config`.
#[derive(Debug, Clone)]
pub struct TierPromotionConfig {
    /// Enable or disable the tier promotion loop.
    pub enabled: bool,
    /// Minimum number of distinct sessions in which a message must appear
    /// before it becomes a promotion candidate.
    pub promotion_min_sessions: u32,
    /// Minimum cosine similarity for two messages to be considered duplicates
    /// eligible for merging into one semantic fact.
    pub similarity_threshold: f32,
    /// How often to run a promotion sweep, in seconds.
    pub sweep_interval_secs: u64,
    /// Maximum number of candidates to process per sweep.
    pub sweep_batch_size: usize,
}

/// Start the background tier promotion loop.
///
/// Each sweep cycle:
/// 1. Fetches episodic candidates with `session_count >= config.promotion_min_sessions`.
/// 2. Embeds candidates and clusters near-duplicates (cosine similarity >= threshold).
/// 3. For each cluster, calls the LLM to merge into a single semantic fact.
/// 4. Validates the merged output; skips the cluster on failure.
/// 5. Promotes validated clusters to semantic tier.
///
/// The loop exits immediately if `config.enabled = false`.
///
/// Database and LLM errors are logged but do not stop the loop.
pub fn start_tier_promotion_loop(
    store: Arc<SqliteStore>,
    provider: AnyProvider,
    config: TierPromotionConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            tracing::debug!("tier promotion disabled (tiers.enabled = false)");
            return;
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs));
        // Skip the first immediate tick so we don't run at startup.
        ticker.tick().await;

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("tier promotion loop shutting down");
                    return;
                }
                _ = ticker.tick() => {}
            }

            tracing::debug!("tier promotion: starting sweep");
            let start = std::time::Instant::now();

            let result = run_promotion_sweep(&store, &provider, &config).await;

            let elapsed_ms = start.elapsed().as_millis();

            match result {
                Ok(stats) => {
                    tracing::info!(
                        candidates = stats.candidates_evaluated,
                        clusters = stats.clusters_formed,
                        promoted = stats.promotions_completed,
                        merge_failures = stats.merge_failures,
                        elapsed_ms,
                        "tier promotion: sweep complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, elapsed_ms, "tier promotion: sweep failed, will retry");
                }
            }
        }
    })
}

/// Stats collected during a single promotion sweep.
#[derive(Debug, Default)]
struct SweepStats {
    candidates_evaluated: usize,
    clusters_formed: usize,
    promotions_completed: usize,
    merge_failures: usize,
}

/// Execute one full promotion sweep cycle.
async fn run_promotion_sweep(
    store: &SqliteStore,
    provider: &AnyProvider,
    config: &TierPromotionConfig,
) -> Result<SweepStats, MemoryError> {
    let candidates = store
        .find_promotion_candidates(config.promotion_min_sessions, config.sweep_batch_size)
        .await?;

    if candidates.is_empty() {
        return Ok(SweepStats::default());
    }

    let mut stats = SweepStats {
        candidates_evaluated: candidates.len(),
        ..SweepStats::default()
    };

    // Embed all candidates for clustering. Skip candidates that fail to embed.
    let mut embedded: Vec<(PromotionCandidate, Vec<f32>)> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !provider.supports_embeddings() {
            // No embedding support — cannot cluster. Promote singletons directly.
            embedded.push((candidate, Vec::new()));
            continue;
        }
        match provider.embed(&candidate.content).await {
            Ok(vec) => embedded.push((candidate, vec)),
            Err(e) => {
                tracing::warn!(
                    message_id = candidate.id.0,
                    error = %e,
                    "tier promotion: failed to embed candidate, skipping"
                );
            }
        }
    }

    if embedded.is_empty() {
        return Ok(stats);
    }

    // Cluster candidates by cosine similarity (greedy nearest-neighbor).
    // Each candidate is assigned to the first existing cluster whose centroid
    // representative has similarity >= threshold with it, or starts a new cluster.
    let threshold = config.similarity_threshold;
    let clusters = cluster_by_similarity(embedded, threshold);

    for cluster in clusters {
        if cluster.len() < 2 {
            // Single-member cluster — no merge needed, skip to avoid unnecessary LLM calls.
            tracing::debug!(
                cluster_size = cluster.len(),
                "tier promotion: singleton cluster skipped"
            );
            continue;
        }

        stats.clusters_formed += 1;

        let source_conv_id = cluster[0].0.conversation_id;

        match merge_cluster_and_promote(store, provider, &cluster, source_conv_id).await {
            Ok(()) => stats.promotions_completed += 1,
            Err(e) => {
                tracing::warn!(
                    cluster_size = cluster.len(),
                    error = %e,
                    "tier promotion: cluster merge failed, skipping"
                );
                stats.merge_failures += 1;
            }
        }
    }

    Ok(stats)
}

/// Cluster candidates by cosine similarity using greedy nearest-neighbor.
///
/// Each candidate is compared to the representative (first member) of existing clusters.
/// If similarity >= threshold, it joins that cluster; otherwise it starts a new one.
/// This is O(n * k) where k is the number of clusters formed, not O(n^2).
fn cluster_by_similarity(
    candidates: Vec<(PromotionCandidate, Vec<f32>)>,
    threshold: f32,
) -> Vec<Vec<(PromotionCandidate, Vec<f32>)>> {
    let mut clusters: Vec<Vec<(PromotionCandidate, Vec<f32>)>> = Vec::new();

    'outer: for candidate in candidates {
        if candidate.1.is_empty() {
            // No embedding — own cluster (will be skipped as singleton).
            clusters.push(vec![candidate]);
            continue;
        }

        for cluster in &mut clusters {
            let rep = &cluster[0].1;
            if rep.is_empty() {
                continue;
            }
            let sim = cosine_similarity(&candidate.1, rep);
            if sim >= threshold {
                cluster.push(candidate);
                continue 'outer;
            }
        }

        clusters.push(vec![candidate]);
    }

    clusters
}

/// Call the LLM to merge a cluster and promote the result to semantic tier.
///
/// Validates the merged output before promoting. If the output is empty or has
/// a cosine similarity below `MERGE_VALIDATION_MIN_SIMILARITY` to all originals,
/// returns an error without promoting.
async fn merge_cluster_and_promote(
    store: &SqliteStore,
    provider: &AnyProvider,
    cluster: &[(PromotionCandidate, Vec<f32>)],
    conversation_id: ConversationId,
) -> Result<(), MemoryError> {
    let contents: Vec<&str> = cluster.iter().map(|(c, _)| c.content.as_str()).collect();
    let original_ids: Vec<crate::types::MessageId> = cluster.iter().map(|(c, _)| c.id).collect();

    let merged = call_merge_llm(provider, &contents).await?;

    // Validate: non-empty result required.
    let merged = merged.trim().to_owned();
    if merged.is_empty() {
        return Err(MemoryError::Other("LLM merge returned empty result".into()));
    }

    // Validate: merged result must be semantically related to at least one original.
    // Embed the merged result and compare against original embeddings.
    if provider.supports_embeddings() {
        let embeddings_available = cluster.iter().any(|(_, emb)| !emb.is_empty());
        if embeddings_available {
            match provider.embed(&merged).await {
                Ok(merged_vec) => {
                    let max_sim = cluster
                        .iter()
                        .filter(|(_, emb)| !emb.is_empty())
                        .map(|(_, emb)| cosine_similarity(&merged_vec, emb))
                        .fold(f32::NEG_INFINITY, f32::max);

                    if max_sim < MERGE_VALIDATION_MIN_SIMILARITY {
                        return Err(MemoryError::Other(format!(
                            "LLM merge validation failed: max similarity to originals = {max_sim:.3} < {MERGE_VALIDATION_MIN_SIMILARITY}"
                        )));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "tier promotion: failed to embed merged result, skipping similarity validation"
                    );
                }
            }
        }
    }

    // Retry the DB write up to 3 times with exponential backoff on SQLITE_BUSY.
    // The LLM merge above is not retried — only the cheap DB write is.
    let delays_ms = [50u64, 100, 200];
    for (attempt, &delay_ms) in delays_ms.iter().enumerate() {
        match store
            .promote_to_semantic(conversation_id, &merged, &original_ids)
            .await
        {
            Ok(_) => break,
            Err(e) => {
                // Detect SQLITE_BUSY via the sqlx::Error::Database error code ("5") when
                // available; fall back to string matching. String matching is safe here because
                // the error originates from SQLite internals, not user input. The fallback
                // handles wrapping layers where downcasting would add disproportionate complexity.
                let is_busy = if let MemoryError::Sqlx(sqlx::Error::Database(ref db_err)) = e {
                    db_err.code().as_deref() == Some("5")
                } else {
                    e.to_string().contains("database is locked")
                };
                if is_busy && attempt < delays_ms.len() - 1 {
                    tracing::warn!(
                        attempt = attempt + 1,
                        delay_ms,
                        "tier promotion: SQLite busy, retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
    tracing::debug!(
        cluster_size = cluster.len(),
        merged_len = merged.len(),
        "tier promotion: cluster promoted to semantic"
    );

    Ok(())
}

/// Call the LLM to distill a set of episodic messages into a single semantic fact.
async fn call_merge_llm(provider: &AnyProvider, contents: &[&str]) -> Result<String, MemoryError> {
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    let bullet_list: String = contents
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {c}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");

    let system_content = "You are a memory consolidation agent. \
        Merge the following episodic memories into a single concise semantic fact. \
        Strip timestamps, session context, hedging, and filler. \
        Output ONLY the distilled fact as a single plain-text sentence or short paragraph. \
        Do not add prefixes like 'The user...' or 'Fact:'.";

    let user_content =
        format!("Merge these episodic memories into one semantic fact:\n\n{bullet_list}");

    let messages = vec![
        Message {
            role: Role::System,
            content: system_content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: user_content,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let timeout = Duration::from_secs(15);

    let result = tokio::time::timeout(timeout, provider.chat(&messages))
        .await
        .map_err(|_| MemoryError::Other("LLM merge timed out after 15s".into()))?
        .map_err(MemoryError::Llm)?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_by_similarity_groups_identical() {
        // Two identical unit vectors should cluster together at any threshold <= 1.0.
        let v1 = vec![1.0f32, 0.0, 0.0];
        let v2 = vec![1.0f32, 0.0, 0.0];
        let v3 = vec![0.0f32, 1.0, 0.0]; // orthogonal

        let candidates = vec![
            (make_candidate(1), v1),
            (make_candidate(2), v2),
            (make_candidate(3), v3),
        ];

        let clusters = cluster_by_similarity(candidates, 0.92f32);
        assert_eq!(clusters.len(), 2, "should produce 2 clusters");
        assert_eq!(clusters[0].len(), 2, "first cluster should have 2 members");
        assert_eq!(clusters[1].len(), 1, "second cluster is the orthogonal one");
    }

    #[test]
    fn cluster_by_similarity_empty_embeddings_become_singletons() {
        let candidates = vec![(make_candidate(1), vec![]), (make_candidate(2), vec![])];
        let clusters = cluster_by_similarity(candidates, 0.92);
        assert_eq!(clusters.len(), 2);
    }

    fn make_candidate(id: i64) -> PromotionCandidate {
        PromotionCandidate {
            id: crate::types::MessageId(id),
            conversation_id: ConversationId(1),
            content: format!("content {id}"),
            session_count: 3,
            importance_score: 0.5,
        }
    }
}
