// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `TiMem` temporal-hierarchical memory tree consolidation (#2262).
//!
//! Background loop that clusters unconsolidated leaf nodes by cosine similarity and merges
//! each cluster into a parent node via LLM summarization.
//!
//! # Transaction safety (critic S2)
//!
//! Each cluster merge runs in its own transaction via `mark_nodes_consolidated`.
//! The full sweep never holds a write lock across multiple clusters.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, Role};

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::store::memory_tree::MemoryTreeRow;
use zeph_common::math::cosine_similarity;

const MERGE_SYSTEM_PROMPT: &str = "\
You are a memory consolidation assistant. Given several related memory nodes, produce a single \
concise summary that captures the essential information from all of them. \
Keep it to 2-4 sentences. Do not repeat details already captured in a single sentence. \
Return only the summary text — no JSON, no preamble.";

/// Configuration for the tree consolidation loop.
pub struct TreeConsolidationConfig {
    pub enabled: bool,
    pub sweep_interval_secs: u64,
    pub batch_size: usize,
    pub similarity_threshold: f32,
    pub max_level: u32,
    pub min_cluster_size: usize,
}

/// Result of one consolidation sweep.
#[derive(Debug, Default)]
pub struct TreeConsolidationResult {
    pub clusters_merged: u32,
    pub nodes_created: u32,
}

/// Start the background tree consolidation loop.
///
/// The loop exits immediately when `config.enabled = false` or `cancel` fires.
pub fn start_tree_consolidation_loop(
    store: Arc<SqliteStore>,
    provider: AnyProvider,
    config: TreeConsolidationConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            tracing::debug!("tree consolidation disabled (tree.enabled = false)");
            return;
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs));
        // Skip the first immediate tick to avoid running at startup.
        ticker.tick().await;

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("tree consolidation loop shutting down");
                    return;
                }
                _ = ticker.tick() => {}
            }

            tracing::debug!("tree consolidation: starting sweep");
            let start = std::time::Instant::now();

            let result = run_tree_consolidation_sweep(&store, &provider, &config).await;
            let elapsed_ms = start.elapsed().as_millis();

            match result {
                Ok(r) => tracing::info!(
                    clusters_merged = r.clusters_merged,
                    nodes_created = r.nodes_created,
                    elapsed_ms,
                    "tree consolidation: sweep complete"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    elapsed_ms,
                    "tree consolidation: sweep failed, will retry"
                ),
            }
        }
    })
}

/// Execute one full consolidation sweep: leaves → level 1, then level 1 → level 2, etc.
///
/// Each cluster runs inside its own transaction (critic S2).
///
/// # Errors
///
/// Returns an error if a database query fails.
pub async fn run_tree_consolidation_sweep(
    store: &SqliteStore,
    provider: &AnyProvider,
    config: &TreeConsolidationConfig,
) -> Result<TreeConsolidationResult, MemoryError> {
    let mut result = TreeConsolidationResult::default();

    for level in 0..config.max_level {
        let candidates = if level == 0 {
            store
                .load_tree_leaves_unconsolidated(config.batch_size)
                .await?
        } else {
            store
                .load_tree_level(i64::from(level), config.batch_size)
                .await?
        };

        if candidates.len() < config.min_cluster_size {
            continue;
        }

        if !provider.supports_embeddings() {
            tracing::debug!(
                "tree consolidation: provider has no embedding support, skipping level {level}"
            );
            continue;
        }

        let embedded = embed_candidates(provider, &candidates).await;
        if embedded.len() < config.min_cluster_size {
            continue;
        }

        let clusters = cluster_by_similarity(
            &embedded,
            config.similarity_threshold,
            config.min_cluster_size,
        );

        for cluster in clusters {
            if cluster.len() < config.min_cluster_size {
                continue;
            }

            let child_ids: Vec<i64> = cluster.iter().map(|(id, _, _)| *id).collect();
            let contents: Vec<&str> = cluster
                .iter()
                .map(|(_, content, _)| content.as_str())
                .collect();

            let summary = match merge_via_llm(provider, &contents).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        level,
                        child_count = cluster.len(),
                        "tree consolidation: LLM merge failed, skipping cluster"
                    );
                    continue;
                }
            };

            if summary.is_empty() {
                continue;
            }

            let token_count = i64::try_from(summary.split_whitespace().count()).unwrap_or(i64::MAX);
            let source_ids_json =
                serde_json::to_string(&child_ids).unwrap_or_else(|_| "[]".to_string());

            // Atomic cluster consolidation: INSERT parent + UPDATE children in one transaction.
            match store
                .consolidate_cluster(
                    i64::from(level + 1),
                    &summary,
                    &source_ids_json,
                    token_count,
                    &child_ids,
                )
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        level,
                        child_count = cluster.len(),
                        "tree consolidation: cluster persist failed, skipping"
                    );
                    continue;
                }
            }

            result.clusters_merged += 1;
            result.nodes_created += 1;
        }
    }

    if result.nodes_created > 0 {
        let _ = store.increment_tree_consolidation_count().await;
    }

    Ok(result)
}

/// Concurrency cap for embed calls — matches `embed_concurrency` default (#2677).
const EMBED_CONCURRENCY: usize = 8;

async fn embed_candidates(
    provider: &AnyProvider,
    candidates: &[MemoryTreeRow],
) -> Vec<(i64, String, Vec<f32>)> {
    let mut embedded = Vec::with_capacity(candidates.len());

    // Process in bounded batches to avoid saturating the embed provider (#2677).
    for chunk in candidates.chunks(EMBED_CONCURRENCY) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|row| {
                let id = row.id;
                let content = row.content.clone();
                async move { (id, content.clone(), provider.embed(&content).await) }
            })
            .collect();

        let results = futures::future::join_all(futures).await;
        for (id, content, result) in results {
            match result {
                Ok(vec) => embedded.push((id, content, vec)),
                Err(e) => tracing::warn!(
                    node_id = id,
                    error = %e,
                    "tree consolidation: failed to embed node, skipping"
                ),
            }
        }
    }
    embedded
}

// INVARIANT: `embedded` must be ordered by `created_at ASC` (as returned by
// `load_tree_leaves_unconsolidated` / `load_tree_level`).  The greedy leader-based algorithm
// is deterministic only when the input order is stable across sweeps.  Do not sort or shuffle
// the slice before calling this function.
fn cluster_by_similarity(
    embedded: &[(i64, String, Vec<f32>)],
    threshold: f32,
    min_cluster_size: usize,
) -> Vec<Vec<(i64, String, Vec<f32>)>> {
    let n = embedded.len();
    let mut assigned = vec![false; n];
    let mut clusters: Vec<Vec<(i64, String, Vec<f32>)>> = Vec::new();

    for i in 0..n {
        if assigned[i] {
            continue;
        }
        let mut cluster = vec![embedded[i].clone()];
        assigned[i] = true;

        for j in (i + 1)..n {
            if assigned[j] {
                continue;
            }
            let sim = cosine_similarity(&embedded[i].2, &embedded[j].2);
            if sim >= threshold {
                cluster.push(embedded[j].clone());
                assigned[j] = true;
            }
        }

        if cluster.len() >= min_cluster_size {
            clusters.push(cluster);
        }
    }

    clusters
}

async fn merge_via_llm(provider: &AnyProvider, contents: &[&str]) -> Result<String, MemoryError> {
    let mut user_prompt = String::from("Memory nodes to consolidate:\n");
    for (i, content) in contents.iter().enumerate() {
        use std::fmt::Write as _;
        let _ = writeln!(user_prompt, "[{}] {}", i + 1, content);
    }
    user_prompt.push_str("\nProduce a concise summary.");

    let llm_messages = [
        Message::from_legacy(Role::System, MERGE_SYSTEM_PROMPT),
        Message::from_legacy(Role::User, user_prompt),
    ];

    let response = provider
        .chat(&llm_messages)
        .await
        .map_err(MemoryError::Llm)?;

    Ok(response.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_by_similarity_groups_identical_vectors() {
        let v1 = vec![1.0f32, 0.0, 0.0];
        let v2 = vec![1.0f32, 0.0, 0.0];
        let v3 = vec![0.0f32, 1.0, 0.0]; // orthogonal

        let embedded = vec![
            (1i64, "a".to_string(), v1),
            (2i64, "b".to_string(), v2),
            (3i64, "c".to_string(), v3),
        ];

        let clusters = cluster_by_similarity(&embedded, 0.9, 2);
        assert_eq!(
            clusters.len(),
            1,
            "identical vectors should form one cluster"
        );
        assert_eq!(clusters[0].len(), 2);
    }

    #[test]
    fn cluster_by_similarity_min_cluster_size_gate() {
        let v1 = vec![1.0f32, 0.0];
        let v2 = vec![1.0f32, 0.0];

        let embedded = vec![(1i64, "a".to_string(), v1), (2i64, "b".to_string(), v2)];

        // Require 3 — no cluster should form.
        let clusters = cluster_by_similarity(&embedded, 0.9, 3);
        assert!(clusters.is_empty());
    }

    #[test]
    fn cluster_by_similarity_no_duplicates_across_clusters() {
        let v = vec![1.0f32, 0.0];
        let embedded = vec![
            (1i64, "a".to_string(), v.clone()),
            (2i64, "b".to_string(), v.clone()),
            (3i64, "c".to_string(), v.clone()),
        ];

        let clusters = cluster_by_similarity(&embedded, 0.9, 2);
        let total_items: usize = clusters.iter().map(Vec::len).sum();
        // All items across all clusters are unique (no double-assignment).
        assert_eq!(total_items, 3);
    }
}
