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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

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
#[derive(Clone)]
pub struct TreeConsolidationConfig {
    /// Enable or disable the tree consolidation background loop.
    pub enabled: bool,
    /// Interval between consolidation sweeps, in seconds.
    pub sweep_interval_secs: u64,
    /// Maximum number of leaf nodes processed per sweep.
    pub batch_size: usize,
    /// Cosine similarity threshold for clustering nodes (0.0–1.0). Nodes with similarity
    /// above this value are merged into a parent node.
    pub similarity_threshold: f32,
    /// Maximum depth of the memory tree (levels above leaf nodes).
    pub max_level: u32,
    /// Minimum cluster size required to trigger LLM consolidation.
    pub min_cluster_size: usize,
    /// Per-call timeout for every `embed()` invocation, in seconds. Default: `5`.
    pub embed_timeout_secs: u64,
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
pub async fn start_tree_consolidation_loop(
    store: Arc<SqliteStore>,
    provider: AnyProvider,
    config: TreeConsolidationConfig,
    cancel: CancellationToken,
) {
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

        let candidate_ids: Vec<i64> = candidates.iter().map(|row| row.id).collect();

        let embedded = embed_candidates(
            provider,
            &candidates,
            Duration::from_secs(config.embed_timeout_secs),
        )
        .await;
        if embedded.len() < config.min_cluster_size {
            // Bumps every loaded candidate, including ones that embedded fine but were left
            // without enough surviving peers to reach `min_cluster_size` — a peer's transient
            // embed failure costs them one attempt too. Harmless one-step bias in practice
            // (`min_cluster_size` is small, so this only fires when nearly all embeds fail).
            bump_stuck_attempts(store, &candidate_ids, level).await;
            continue;
        }

        let clusters = cluster_by_similarity(
            &embedded,
            config.similarity_threshold,
            config.min_cluster_size,
        );

        let consolidated_ids =
            merge_clusters(store, provider, clusters, level, config, &mut result).await;

        // Nodes that end up consolidated this sweep drop out of the load query naturally
        // (`parent_id` gets set); everything else keeps failing to cluster and must have its
        // attempt count bumped so the next sweep's load query deprioritizes it (#6393).
        let stuck_ids: Vec<i64> = candidate_ids
            .into_iter()
            .filter(|id| !consolidated_ids.contains(id))
            .collect();
        bump_stuck_attempts(store, &stuck_ids, level).await;
    }

    if result.nodes_created > 0 {
        let _ = store.increment_tree_consolidation_count().await;
    }

    Ok(result)
}

/// Merge every cluster of at least `config.min_cluster_size` nodes into a parent node via LLM
/// summarization, updating `result` in place. Returns the set of child node ids that were
/// actually consolidated (persisted successfully) — everything else in the level's candidate
/// set is left for the caller to mark as a failed attempt (#6393).
async fn merge_clusters(
    store: &SqliteStore,
    provider: &AnyProvider,
    clusters: Vec<Vec<(i64, String, Vec<f32>)>>,
    level: u32,
    config: &TreeConsolidationConfig,
    result: &mut TreeConsolidationResult,
) -> HashSet<i64> {
    let mut consolidated_ids: HashSet<i64> = HashSet::new();

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
            Ok(_) => {
                consolidated_ids.extend(&child_ids);
            }
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

    consolidated_ids
}

/// Bump the consolidation-attempt counter for nodes that were loaded this sweep but did not
/// end up consolidated, so the next sweep's load query deprioritizes them (#6393). Best-effort:
/// a failure here does not fail the sweep, it only means those nodes stay at their current
/// attempt count and may be reloaded sooner than intended on the next sweep.
async fn bump_stuck_attempts(store: &SqliteStore, node_ids: &[i64], level: u32) {
    if node_ids.is_empty() {
        return;
    }
    if let Err(e) = store.bump_consolidation_attempts(node_ids).await {
        tracing::warn!(
            error = %e,
            level,
            count = node_ids.len(),
            "tree consolidation: failed to bump consolidation attempts"
        );
    }
}

/// Concurrency cap for embed calls — matches `embed_concurrency` default (#2677).
const EMBED_CONCURRENCY: usize = 8;

async fn embed_candidates(
    provider: &AnyProvider,
    candidates: &[MemoryTreeRow],
    embed_timeout: Duration,
) -> Vec<(i64, String, Vec<f32>)> {
    let mut embedded = Vec::with_capacity(candidates.len());

    // Process in bounded batches to avoid saturating the embed provider (#2677).
    for chunk in candidates.chunks(EMBED_CONCURRENCY) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|row| {
                let id = row.id;
                let content = row.content.clone();
                async move {
                    let result =
                        tokio::time::timeout(embed_timeout, provider.embed(&content)).await;
                    let result = match result {
                        Ok(r) => r,
                        Err(_elapsed) => {
                            tracing::warn!(
                                node_id = id,
                                "tree consolidation: embed() timed out, skipping node"
                            );
                            return (id, content, Err(zeph_llm::error::LlmError::Timeout));
                        }
                    };
                    (id, content, result)
                }
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

    /// `embed()` timeout in `embed_candidates` → timed-out nodes are dropped, function returns
    /// only successfully embedded entries (fail-open: the sweep can still proceed with fewer nodes).
    #[tokio::test]
    async fn embed_candidates_timeout_drops_timed_out_nodes() {
        let slow = zeph_llm::any::AnyProvider::Mock(
            zeph_llm::mock::MockProvider::default().with_embed_delay(10_000),
        );

        let candidates = vec![
            MemoryTreeRow {
                id: 1,
                level: 0,
                parent_id: None,
                content: "Alice prefers Rust".to_owned(),
                source_ids: "[]".to_owned(),
                token_count: 3,
                consolidated_at: None,
                created_at: "2026-01-01T00:00:00".to_owned(),
            },
            MemoryTreeRow {
                id: 2,
                level: 0,
                parent_id: None,
                content: "Alice likes async code".to_owned(),
                source_ids: "[]".to_owned(),
                token_count: 4,
                consolidated_at: None,
                created_at: "2026-01-01T00:00:01".to_owned(),
            },
        ];

        tokio::time::pause();

        let fut = embed_candidates(&slow, &candidates, Duration::from_secs(5));
        let (result, ()) = tokio::join!(fut, async {
            tokio::time::advance(std::time::Duration::from_secs(6)).await;
        });

        assert!(
            result.is_empty(),
            "all nodes must be dropped on embed timeout, got {} entries",
            result.len()
        );
    }

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

    async fn make_store() -> SqliteStore {
        SqliteStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory store")
    }

    fn test_config() -> TreeConsolidationConfig {
        TreeConsolidationConfig {
            enabled: true,
            sweep_interval_secs: 3600,
            batch_size: 2,
            similarity_threshold: 0.9,
            max_level: 1,
            min_cluster_size: 2,
            embed_timeout_secs: 5,
        }
    }

    /// Backdates `last_attempted_at` for `ids` to an exact, controlled value — used to build
    /// deterministic multi-sweep timelines in tests without depending on real wall-clock gaps.
    async fn backdate_last_attempted(store: &SqliteStore, ids: &[i64], timestamp: &str) {
        for &id in ids {
            zeph_db::query(zeph_db::sql!(
                "UPDATE memory_tree SET last_attempted_at = ? WHERE id = ?"
            ))
            .bind(timestamp)
            .bind(id)
            .execute(store.pool())
            .await
            .expect("backdate last_attempted_at");
        }
    }

    /// Backdates `created_at` for `ids` — see [`backdate_last_attempted`].
    async fn backdate_created_at(store: &SqliteStore, ids: &[i64], timestamp: &str) {
        for &id in ids {
            zeph_db::query(zeph_db::sql!(
                "UPDATE memory_tree SET created_at = ? WHERE id = ?"
            ))
            .bind(timestamp)
            .bind(id)
            .execute(store.pool())
            .await
            .expect("backdate created_at");
        }
    }

    /// #6393 regression, hardened per code review (2026-07-17T18-50-48-review.md Critical #1):
    /// verifies `run_tree_consolidation_sweep` (the real production code path) wires
    /// `bump_stuck_attempts` correctly when a cluster stays below `min_cluster_size`, and that
    /// the resulting deprioritization is not permanent. The original version of this test
    /// asserted a fresh leaf wins the very next load immediately after one bump — false under
    /// real timing (a just-bumped leaf's touch is the freshest in the table and correctly keeps
    /// winning until it is left behind by a later sweep); this version backdates the sweep's
    /// own bump explicitly, so the assertion holds regardless of how fast the test executes.
    #[tokio::test]
    async fn sweep_bumps_attempts_and_stale_stuck_batch_does_not_starve_new_leaf() {
        let store = make_store().await;
        let config = test_config();

        let leaf1 = store.insert_tree_leaf("stuck one", 5).await.expect("l1");
        let leaf2 = store.insert_tree_leaf("stuck two", 5).await.expect("l2");

        // One of the two embed calls fails, so `embedded.len() == 1 < min_cluster_size == 2`:
        // both loaded candidates end up unconsolidated ("stuck") this sweep.
        let provider = AnyProvider::Mock(
            zeph_llm::mock::MockProvider::default()
                .with_embedding(vec![1.0, 0.0])
                .with_errors(vec![zeph_llm::error::LlmError::Timeout]),
        );

        run_tree_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep");

        // Both stuck leaves must still be present and unconsolidated (no cluster formed) —
        // proves the sweep ran the expected embed-failure path and called `bump_stuck_attempts`.
        let stuck_ids: std::collections::HashSet<i64> = store
            .load_tree_leaves_unconsolidated(10)
            .await
            .expect("load")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(stuck_ids.contains(&leaf1));
        assert!(stuck_ids.contains(&leaf2));

        // The sweep's own `bump_stuck_attempts` call (the production wiring verified above)
        // already set `last_attempted_at` to real "now". Pin it to a controlled, known value
        // (T=1) so the rest of this test is fully deterministic regardless of how fast it runs.
        backdate_last_attempted(&store, &[leaf1, leaf2], "2000-01-01 00:00:01").await;

        // A new leaf created strictly *after* that touch (T=2) does not win the very next
        // load — a just-touched leaf's timestamp is still older (smaller) than anything created
        // right after it, so leaf1/leaf2 correctly keep winning for now. This is the real,
        // "not immediate" property: a bump does not instantly lose to a leaf created around the
        // same moment (code review #6393 Critical #1 — the original version of this test
        // asserted the opposite and only passed via a same-second timestamp tie).
        let fresh = store.insert_tree_leaf("fresh leaf", 5).await.expect("l3");
        backdate_created_at(&store, &[fresh], "2000-01-01 00:00:02").await;

        let immediate: std::collections::HashSet<i64> = store
            .load_tree_leaves_unconsolidated(1)
            .await
            .expect("load immediate")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(
            immediate == std::collections::HashSet::from([leaf1])
                || immediate == std::collections::HashSet::from([leaf2]),
            "a just-touched stuck leaf must still outrank a leaf created immediately \
             afterward, got {immediate:?}"
        );

        // A second sweep-equivalent touch of leaf1/leaf2 (T=3), still without `fresh` ever
        // being touched, pushes their timestamp past `fresh`'s frozen T=2 — `fresh` must then
        // win: the bounded, real multi-sweep starvation guard, not a false single-bump
        // immediacy claim.
        backdate_last_attempted(&store, &[leaf1, leaf2], "2000-01-01 00:00:03").await;
        let next_batch = store
            .load_tree_leaves_unconsolidated(1)
            .await
            .expect("load next batch");
        assert_eq!(
            next_batch.len(),
            1,
            "batch_size limit of 1 must be respected"
        );
        assert_eq!(
            next_batch[0].id, fresh,
            "a leaf that predates the stuck batch's most recent touch must win once it is \
             touched again without the fresh leaf ever being touched itself"
        );
    }

    /// #6393: normal clustering (leaves that DO cluster successfully) must behave exactly as
    /// before — the new attempts tracking must not interfere when a cluster actually forms.
    #[tokio::test]
    async fn sweep_still_merges_a_successfully_clustered_batch() {
        let store = make_store().await;
        let config = test_config();

        store.insert_tree_leaf("alpha", 5).await.expect("l1");
        store.insert_tree_leaf("beta", 5).await.expect("l2");

        // Both leaves embed to the same vector, so they merge into one cluster of size 2.
        let provider = AnyProvider::Mock(
            zeph_llm::mock::MockProvider::default().with_embedding(vec![1.0, 0.0]),
        );

        let result = run_tree_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep");

        assert_eq!(result.clusters_merged, 1);
        assert_eq!(result.nodes_created, 1);

        let leaves = store
            .load_tree_leaves_unconsolidated(10)
            .await
            .expect("load");
        assert!(
            leaves.is_empty(),
            "successfully clustered leaves must be consolidated, not left as candidates"
        );
    }
}
