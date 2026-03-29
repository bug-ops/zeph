// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! All-Mem lifelong memory consolidation (#2270).
//!
//! Provides a background sweep loop that periodically clusters semantically similar messages
//! and merges them into consolidated entries via LLM. Originals are never deleted — they are
//! marked as consolidated (`consolidated = 1`) and deprioritized in recall over time via
//! temporal decay.
//!
//! # Transaction safety
//!
//! Every `MERGE` operation runs inside a single `SQLite` transaction via
//! [`DbStore::apply_consolidation_merge`]. Partial state is never written.
//!
//! # Clustering
//!
//! Uses in-memory cosine similarity over the batch (same pattern as `tiers.rs`), not Qdrant.
//! This keeps the feature independent of optional infrastructure.

use std::sync::Arc;
use std::time::Duration;
#[allow(unused_imports)]
use zeph_db::sql;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::error::MemoryError;
use crate::math::cosine_similarity;
use crate::store::SqliteStore;

/// Topology operation proposed by the LLM for memory consolidation.
///
/// MVP includes `Merge` and `Update` only. `Split` is deferred — the trigger
/// condition (a single entry being "too broad") requires a separate sweep strategy
/// not based on similarity clustering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TopologyOp {
    /// Merge N similar messages into one consolidated entry.
    Merge {
        source_ids: Vec<i64>,
        merged_content: String,
        confidence: f32,
    },
    /// Update/refine an existing consolidated entry with new evidence.
    Update {
        target_id: i64,
        new_content: String,
        additional_source_ids: Vec<i64>,
        confidence: f32,
    },
}

/// Result of a single consolidation sweep cycle.
#[derive(Debug, Default)]
pub struct ConsolidationResult {
    pub merges: u32,
    pub updates: u32,
    /// Ops skipped because their confidence was below the threshold.
    pub skipped: u32,
}

/// Configuration for the consolidation sweep, passed from `zeph-config::ConsolidationConfig`.
///
/// Defined locally to avoid a dependency from `zeph-memory` on `zeph-config`.
#[derive(Debug, Clone)]
pub struct ConsolidationConfig {
    pub enabled: bool,
    pub confidence_threshold: f32,
    pub sweep_interval_secs: u64,
    pub sweep_batch_size: usize,
    pub similarity_threshold: f32,
}

/// Start the background consolidation loop.
///
/// Each sweep cycle:
/// 1. Fetches all conversations with unconsolidated messages.
/// 2. For each conversation, loads a batch of unconsolidated messages.
/// 3. Embeds candidates and clusters near-duplicates (cosine similarity >= threshold).
/// 4. For each cluster with >= 2 messages, calls the LLM to produce a merged fact.
/// 5. Applies accepted merges inside transactions.
///
/// The loop exits immediately if `config.enabled = false`.
///
/// Database and LLM errors are logged but do not stop the loop.
pub fn start_consolidation_loop(
    store: Arc<SqliteStore>,
    provider: AnyProvider,
    config: ConsolidationConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            tracing::debug!("consolidation disabled (consolidation.enabled = false)");
            return;
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs));
        // Skip the first immediate tick to avoid running at startup.
        ticker.tick().await;

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("consolidation loop shutting down");
                    return;
                }
                _ = ticker.tick() => {}
            }

            tracing::debug!("consolidation: starting sweep");
            let start = std::time::Instant::now();

            let result = run_consolidation_sweep(&store, &provider, &config).await;
            let elapsed_ms = start.elapsed().as_millis();

            match result {
                Ok(r) => {
                    if r.skipped > 0 && r.merges + r.updates == 0 {
                        tracing::warn!(
                            skipped = r.skipped,
                            elapsed_ms,
                            "consolidation: all proposed ops below confidence threshold — \
                             consider lowering confidence_threshold or checking provider quality"
                        );
                    } else {
                        tracing::info!(
                            merges = r.merges,
                            updates = r.updates,
                            skipped = r.skipped,
                            elapsed_ms,
                            "consolidation: sweep complete"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, elapsed_ms, "consolidation: sweep failed, will retry");
                }
            }
        }
    })
}

/// Execute one full consolidation sweep cycle.
///
/// # Errors
///
/// Returns an error if a database query fails. LLM errors for individual clusters are
/// logged and skipped without propagating.
#[allow(clippy::too_many_lines)]
pub async fn run_consolidation_sweep(
    store: &SqliteStore,
    provider: &AnyProvider,
    config: &ConsolidationConfig,
) -> Result<ConsolidationResult, MemoryError> {
    let mut result = ConsolidationResult::default();

    // Find all conversations that have unconsolidated messages.
    let conv_ids = store.conversations_with_unconsolidated_messages().await?;

    for conv_id in conv_ids {
        let candidates = store
            .find_unconsolidated_messages(conv_id, config.sweep_batch_size)
            .await?;

        if candidates.is_empty() {
            continue;
        }

        // Embed all candidates for clustering.
        let mut embedded: Vec<(i64, String, Vec<f32>)> = Vec::with_capacity(candidates.len());
        for (id, content) in candidates {
            if !provider.supports_embeddings() {
                // No embedding support — cannot cluster, skip this conversation.
                break;
            }
            match provider.embed(&content).await {
                Ok(vec) => embedded.push((id.0, content, vec)),
                Err(e) => {
                    tracing::warn!(
                        message_id = id.0,
                        error = %e,
                        "consolidation: failed to embed candidate, skipping"
                    );
                }
            }
        }

        if embedded.len() < 2 {
            continue;
        }

        let clusters = cluster_by_similarity(&embedded, config.similarity_threshold);

        for cluster in clusters {
            if cluster.len() < 2 {
                continue;
            }

            let ops = propose_merge_op(provider, &cluster).await;
            match ops {
                None => {
                    tracing::debug!(
                        cluster_size = cluster.len(),
                        "consolidation: LLM returned no op for cluster, skipping"
                    );
                }
                Some(TopologyOp::Merge {
                    source_ids,
                    merged_content,
                    confidence,
                }) => {
                    let source_msg_ids: Vec<crate::types::MessageId> = source_ids
                        .iter()
                        .map(|&id| crate::types::MessageId(id))
                        .collect();
                    match store
                        .apply_consolidation_merge(
                            conv_id,
                            "assistant",
                            &merged_content,
                            &source_msg_ids,
                            confidence,
                            config.confidence_threshold,
                        )
                        .await
                    {
                        Ok(true) => result.merges += 1,
                        Ok(false) => result.skipped += 1,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                cluster_size = cluster.len(),
                                "consolidation: merge failed"
                            );
                        }
                    }
                }
                Some(TopologyOp::Update {
                    new_content,
                    additional_source_ids,
                    confidence,
                    ..
                }) => {
                    // Update: same apply path but with a fresh merge for MVP simplicity.
                    let source_msg_ids: Vec<crate::types::MessageId> = additional_source_ids
                        .iter()
                        .map(|&id| crate::types::MessageId(id))
                        .collect();
                    match store
                        .apply_consolidation_merge(
                            conv_id,
                            "assistant",
                            &new_content,
                            &source_msg_ids,
                            confidence,
                            config.confidence_threshold,
                        )
                        .await
                    {
                        Ok(true) => result.updates += 1,
                        Ok(false) => result.skipped += 1,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "consolidation: update failed"
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(result)
}

/// A cluster: (representative embedding, list of (id, content) members).
type Cluster = (Vec<f32>, Vec<(i64, String)>);

/// Cluster messages by cosine similarity using greedy nearest-neighbor.
///
/// Each message is assigned to the first existing cluster whose representative has
/// cosine similarity >= `threshold` with it, or starts a new cluster.
fn cluster_by_similarity(
    embedded: &[(i64, String, Vec<f32>)],
    threshold: f32,
) -> Vec<Vec<(i64, String)>> {
    let mut clusters: Vec<Cluster> = Vec::new();

    for (id, content, embedding) in embedded {
        let mut assigned = false;
        for (rep_emb, members) in &mut clusters {
            if cosine_similarity(embedding, rep_emb) >= threshold {
                members.push((*id, content.clone()));
                assigned = true;
                break;
            }
        }
        if !assigned {
            clusters.push((embedding.clone(), vec![(*id, content.clone())]));
        }
    }

    clusters.into_iter().map(|(_, members)| members).collect()
}

/// Ask the LLM to produce a `TopologyOp` for a cluster of similar messages.
///
/// Returns `None` if the LLM response cannot be parsed or if the LLM declines.
async fn propose_merge_op(provider: &AnyProvider, cluster: &[(i64, String)]) -> Option<TopologyOp> {
    use zeph_llm::provider::{Message, Role};

    let entries: String = cluster
        .iter()
        .map(|(id, content)| format!("  [id={id}] {content}"))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are a memory consolidation assistant. \
         The following messages are semantically similar and should be consolidated.\n\n\
         Messages:\n{entries}\n\n\
         Produce a single JSON object representing a consolidation operation.\n\
         Use this exact schema (choose either 'merge' or 'update'):\n\
         {{\"op\":\"merge\",\"source_ids\":[<list of ids>],\"merged_content\":\"<combined fact>\",\"confidence\":<0.0-1.0>}}\n\
         OR\n\
         {{\"op\":\"update\",\"target_id\":<id>,\"new_content\":\"<updated fact>\",\"additional_source_ids\":[<ids>],\"confidence\":<0.0-1.0>}}\n\n\
         Return ONLY the JSON object, no explanation."
    );

    let messages = vec![Message::from_legacy(Role::User, &prompt)];
    let text = match provider.chat(&messages).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "consolidation: LLM call failed");
            return None;
        }
    };

    // Try to parse from the first JSON object in the response.
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let json_slice = &text[start..=end];

    match serde_json::from_str::<TopologyOp>(json_slice) {
        Ok(op) => Some(op),
        Err(e) => {
            tracing::debug!(error = %e, response = %json_slice, "consolidation: failed to parse LLM response as TopologyOp");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_op_merge_serde_roundtrip() {
        let op = TopologyOp::Merge {
            source_ids: vec![1, 2, 3],
            merged_content: "Alice uses Rust and loves neovim".into(),
            confidence: 0.9,
        };
        let json = serde_json::to_string(&op).unwrap();
        let restored: TopologyOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, restored);
    }

    #[test]
    fn topology_op_update_serde_roundtrip() {
        let op = TopologyOp::Update {
            target_id: 5,
            new_content: "Alice prefers Rust over Python".into(),
            additional_source_ids: vec![6, 7],
            confidence: 0.85,
        };
        let json = serde_json::to_string(&op).unwrap();
        let restored: TopologyOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, restored);
    }

    #[test]
    fn cluster_by_similarity_groups_identical_embeddings() {
        // Two identical embeddings should cluster together.
        let emb = vec![1.0_f32, 0.0, 0.0];
        let entries = vec![
            (1i64, "msg1".into(), emb.clone()),
            (2i64, "msg2".into(), emb.clone()),
            (3i64, "orthogonal".into(), vec![0.0, 1.0, 0.0]),
        ];
        let clusters = cluster_by_similarity(&entries, 0.9);
        // msg1 and msg2 should be in the same cluster; orthogonal in its own.
        assert_eq!(clusters.len(), 2);
        let sizes: Vec<usize> = {
            let mut s: Vec<usize> = clusters.iter().map(Vec::len).collect();
            s.sort_unstable();
            s
        };
        assert_eq!(sizes, vec![1, 2]);
    }

    #[test]
    fn cluster_by_similarity_all_orthogonal_gives_singletons() {
        let entries = vec![
            (1i64, "a".into(), vec![1.0_f32, 0.0, 0.0]),
            (2i64, "b".into(), vec![0.0, 1.0, 0.0]),
            (3i64, "c".into(), vec![0.0, 0.0, 1.0]),
        ];
        let clusters = cluster_by_similarity(&entries, 0.9);
        assert_eq!(clusters.len(), 3);
        for c in &clusters {
            assert_eq!(c.len(), 1);
        }
    }

    #[tokio::test]
    async fn apply_consolidation_merge_inserts_and_marks_sources() {
        use crate::store::SqliteStore;
        let store = SqliteStore::new(":memory:").await.unwrap();
        let conv_id = store.create_conversation().await.unwrap();

        let m1 = store
            .save_message(conv_id, "user", "Alice uses Rust")
            .await
            .unwrap();
        let m2 = store
            .save_message(conv_id, "user", "Alice loves Rust")
            .await
            .unwrap();

        let accepted = store
            .apply_consolidation_merge(
                conv_id,
                "assistant",
                "Alice uses and loves Rust",
                &[m1, m2],
                0.95,
                0.7,
            )
            .await
            .unwrap();
        assert!(
            accepted,
            "merge must be accepted when confidence >= threshold"
        );

        // Verify originals are now marked consolidated.
        let rows: Vec<(i64,)> = zeph_db::query_as(sql!(
            "SELECT consolidated FROM messages WHERE id IN (?, ?) ORDER BY id"
        ))
        .bind(m1)
        .bind(m2)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1, "source m1 must be marked consolidated");
        assert_eq!(rows[1].0, 1, "source m2 must be marked consolidated");

        // Verify join table has entries.
        let join_count: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM memory_consolidation_sources WHERE source_id IN (?, ?)"
        ))
        .bind(m1)
        .bind(m2)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(join_count.0, 2, "both sources must appear in join table");
    }

    #[tokio::test]
    async fn apply_consolidation_merge_skips_below_threshold() {
        use crate::store::SqliteStore;
        let store = SqliteStore::new(":memory:").await.unwrap();
        let conv_id = store.create_conversation().await.unwrap();

        let m1 = store.save_message(conv_id, "user", "foo").await.unwrap();
        let m2 = store.save_message(conv_id, "user", "bar").await.unwrap();

        let accepted = store
            .apply_consolidation_merge(conv_id, "assistant", "combined", &[m1, m2], 0.5, 0.7)
            .await
            .unwrap();
        assert!(
            !accepted,
            "merge must be skipped when confidence < threshold"
        );
    }

    #[tokio::test]
    async fn find_unconsolidated_messages_returns_originals_only() {
        use crate::store::SqliteStore;
        let store = SqliteStore::new(":memory:").await.unwrap();
        let conv_id = store.create_conversation().await.unwrap();

        let m1 = store
            .save_message(conv_id, "user", "original 1")
            .await
            .unwrap();
        let m2 = store
            .save_message(conv_id, "user", "original 2")
            .await
            .unwrap();

        // Merge them so m1 and m2 become consolidated=1.
        store
            .apply_consolidation_merge(conv_id, "assistant", "merged", &[m1, m2], 0.9, 0.7)
            .await
            .unwrap();

        let remaining = store
            .find_unconsolidated_messages(conv_id, 100)
            .await
            .unwrap();
        // The consolidated product (consolidated=1) and originals (now consolidated=1) must not appear.
        for (id, _) in &remaining {
            assert!(
                *id != m1 && *id != m2,
                "consolidated originals must not appear in sweep candidates"
            );
        }
    }

    #[tokio::test]
    async fn find_consolidated_for_source_returns_consolidated_id() {
        use crate::store::SqliteStore;
        let store = SqliteStore::new(":memory:").await.unwrap();
        let conv_id = store.create_conversation().await.unwrap();

        let m1 = store.save_message(conv_id, "user", "fact a").await.unwrap();
        let m2 = store.save_message(conv_id, "user", "fact b").await.unwrap();

        store
            .apply_consolidation_merge(conv_id, "assistant", "fact a and b", &[m1, m2], 0.9, 0.7)
            .await
            .unwrap();

        let found = store.find_consolidated_for_source(m1).await.unwrap();
        assert!(found.is_some(), "must find consolidated entry for m1");

        let not_found = store
            .find_consolidated_for_source(crate::types::MessageId(9999))
            .await
            .unwrap();
        assert!(
            not_found.is_none(),
            "must return None for unknown source_id"
        );
    }

    /// Sweep on an empty DB (no conversations) must return Ok with all-zero counters.
    #[tokio::test]
    async fn run_consolidation_sweep_empty_db_returns_ok() {
        use std::sync::Arc;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        use crate::store::SqliteStore;

        let store = Arc::new(SqliteStore::new(":memory:").await.unwrap());
        let provider = AnyProvider::Mock(MockProvider::default());
        let config = ConsolidationConfig {
            enabled: true,
            confidence_threshold: 0.75,
            sweep_interval_secs: 300,
            sweep_batch_size: 100,
            similarity_threshold: 0.85,
        };

        let result = run_consolidation_sweep(&store, &provider, &config).await;
        let r = result.expect("sweep must not error on empty DB");
        assert_eq!(r.merges, 0);
        assert_eq!(r.updates, 0);
        assert_eq!(r.skipped, 0);
    }

    /// When provider does not support embeddings, sweep skips the conversation
    /// and returns with zero merges (no panic, no error).
    #[tokio::test]
    async fn run_consolidation_sweep_no_embedding_support_skips_gracefully() {
        use std::sync::Arc;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        use crate::store::SqliteStore;

        let store = Arc::new(SqliteStore::new(":memory:").await.unwrap());
        let conv_id = store.create_conversation().await.unwrap();
        store
            .save_message(conv_id, "user", "Alice uses Rust")
            .await
            .unwrap();
        store
            .save_message(conv_id, "user", "Alice loves Rust")
            .await
            .unwrap();

        // MockProvider default has supports_embeddings = false
        let provider = AnyProvider::Mock(MockProvider::default());
        let config = ConsolidationConfig {
            enabled: true,
            confidence_threshold: 0.75,
            sweep_interval_secs: 300,
            sweep_batch_size: 100,
            similarity_threshold: 0.85,
        };

        let result = run_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep must not error when embeddings unsupported");
        assert_eq!(
            result.merges, 0,
            "no merges expected when embeddings unsupported"
        );
    }

    /// `apply_consolidation_merge` with empty source list returns false without writing anything.
    #[tokio::test]
    async fn apply_consolidation_merge_empty_sources_skipped() {
        use crate::store::SqliteStore;
        let store = SqliteStore::new(":memory:").await.unwrap();
        let conv_id = store.create_conversation().await.unwrap();

        let accepted = store
            .apply_consolidation_merge(conv_id, "assistant", "merged", &[], 0.95, 0.7)
            .await
            .unwrap();
        assert!(!accepted, "empty source list must be rejected");

        let count: (i64,) = zeph_db::query_as(sql!("SELECT COUNT(*) FROM messages"))
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count.0, 0, "no rows must be written for empty source list");
    }

    /// `apply_consolidation_merge` at exactly the threshold boundary (confidence == threshold)
    /// must be accepted.
    #[tokio::test]
    async fn apply_consolidation_merge_at_exact_threshold_accepted() {
        use crate::store::SqliteStore;
        let store = SqliteStore::new(":memory:").await.unwrap();
        let conv_id = store.create_conversation().await.unwrap();

        let m1 = store.save_message(conv_id, "user", "a").await.unwrap();
        let m2 = store.save_message(conv_id, "user", "b").await.unwrap();

        let threshold = 0.75_f32;
        let accepted = store
            .apply_consolidation_merge(
                conv_id,
                "assistant",
                "a and b",
                &[m1, m2],
                threshold,
                threshold,
            )
            .await
            .unwrap();
        assert!(
            accepted,
            "merge at exactly the confidence threshold must be accepted"
        );
    }

    /// #2359: transaction must be rolled back when the first INSERT fails
    /// (non-existent `conversation_id` violates FK on `messages.conversation_id`).
    /// After the error: `memory_consolidation_sources` has 0 rows,
    /// source messages remain consolidated = 0.
    #[tokio::test]
    async fn apply_consolidation_merge_rollback_on_mid_tx_error() {
        use crate::store::SqliteStore;
        use crate::types::ConversationId;
        let store = SqliteStore::new(":memory:").await.unwrap();
        let conv_id = store.create_conversation().await.unwrap();

        let m1 = store.save_message(conv_id, "user", "fact x").await.unwrap();
        let m2 = store.save_message(conv_id, "user", "fact y").await.unwrap();

        // Pass a non-existent conversation_id to trigger FK violation on the
        // INSERT INTO messages step, which is the first write inside the transaction.
        let bad_conv = ConversationId(99999);
        let result = store
            .apply_consolidation_merge(bad_conv, "assistant", "merged", &[m1, m2], 0.9, 0.7)
            .await;
        assert!(result.is_err(), "must return Err on FK violation");

        // The transaction must have been rolled back: no rows in the join table.
        let join_count: (i64,) =
            zeph_db::query_as(sql!("SELECT COUNT(*) FROM memory_consolidation_sources"))
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(join_count.0, 0, "join table must be empty after rollback");

        // Original messages must still be unconsolidated.
        let rows: Vec<(i64,)> = zeph_db::query_as(sql!(
            "SELECT consolidated FROM messages WHERE id IN (?, ?) ORDER BY id"
        ))
        .bind(m1)
        .bind(m2)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 0, "m1 must remain consolidated=0 after rollback");
        assert_eq!(rows[1].0, 0, "m2 must remain consolidated=0 after rollback");
    }

    /// #2360: only 1 message in DB — `embedded.len()` < 2 guard fires, all counters stay 0.
    #[tokio::test]
    async fn run_consolidation_sweep_single_candidate_skips() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let store = Arc::new(SqliteStore::new(":memory:").await.unwrap());
        let conv_id = store.create_conversation().await.unwrap();
        store
            .save_message(conv_id, "user", "only one message")
            .await
            .unwrap();

        let mut mock = MockProvider::default();
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);

        let config = ConsolidationConfig {
            enabled: true,
            confidence_threshold: 0.7,
            sweep_interval_secs: 300,
            sweep_batch_size: 100,
            similarity_threshold: 0.85,
        };

        let r = run_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep must not error with single candidate");
        assert_eq!(r.merges, 0);
        assert_eq!(r.updates, 0);
        assert_eq!(r.skipped, 0);
    }

    /// #2360: 2 messages + `MockProvider` returning merge op → assert `r.merges` == 1.
    #[tokio::test]
    async fn run_consolidation_sweep_merge_increments_counter() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let store = Arc::new(SqliteStore::new(":memory:").await.unwrap());
        let conv_id = store.create_conversation().await.unwrap();
        let m1 = store
            .save_message(conv_id, "user", "Alice uses Rust")
            .await
            .unwrap();
        let m2 = store
            .save_message(conv_id, "user", "Alice loves Rust")
            .await
            .unwrap();

        let merge_json = format!(
            r#"{{"op":"merge","source_ids":[{},{}],"merged_content":"Alice uses and loves Rust","confidence":0.95}}"#,
            m1.0, m2.0
        );
        let mut mock = MockProvider::with_responses(vec![merge_json]);
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);

        let config = ConsolidationConfig {
            enabled: true,
            confidence_threshold: 0.7,
            sweep_interval_secs: 300,
            sweep_batch_size: 100,
            similarity_threshold: 0.85,
        };

        let r = run_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep must not error");
        assert_eq!(r.merges, 1, "exactly one merge must be counted");
        assert_eq!(r.updates, 0);
        assert_eq!(r.skipped, 0);
    }

    /// #2360: 2 messages + `MockProvider` returning update op → assert `r.updates` == 1.
    #[tokio::test]
    async fn run_consolidation_sweep_update_increments_counter() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let store = Arc::new(SqliteStore::new(":memory:").await.unwrap());
        let conv_id = store.create_conversation().await.unwrap();
        let m1 = store
            .save_message(conv_id, "user", "Alice uses Rust")
            .await
            .unwrap();
        let m2 = store
            .save_message(conv_id, "user", "Alice loves Rust")
            .await
            .unwrap();

        let update_json = format!(
            r#"{{"op":"update","target_id":{},"new_content":"Alice uses and loves Rust","additional_source_ids":[{}],"confidence":0.92}}"#,
            m1.0, m2.0
        );
        let mut mock = MockProvider::with_responses(vec![update_json]);
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);

        let config = ConsolidationConfig {
            enabled: true,
            confidence_threshold: 0.7,
            sweep_interval_secs: 300,
            sweep_batch_size: 100,
            similarity_threshold: 0.85,
        };

        let r = run_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep must not error");
        assert_eq!(r.updates, 1, "exactly one update must be counted");
        assert_eq!(r.merges, 0);
        assert_eq!(r.skipped, 0);
    }

    /// #2360: `MockProvider` returns merge op with confidence 0.3, threshold is 0.7
    /// → the op is below threshold and `r.skipped` == 1.
    #[tokio::test]
    async fn run_consolidation_sweep_skipped_below_threshold() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let store = Arc::new(SqliteStore::new(":memory:").await.unwrap());
        let conv_id = store.create_conversation().await.unwrap();
        let m1 = store
            .save_message(conv_id, "user", "Alice uses Rust")
            .await
            .unwrap();
        let m2 = store
            .save_message(conv_id, "user", "Alice loves Rust")
            .await
            .unwrap();

        let low_confidence_json = format!(
            r#"{{"op":"merge","source_ids":[{},{}],"merged_content":"merged","confidence":0.3}}"#,
            m1.0, m2.0
        );
        let mut mock = MockProvider::with_responses(vec![low_confidence_json]);
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);

        let config = ConsolidationConfig {
            enabled: true,
            confidence_threshold: 0.7,
            sweep_interval_secs: 300,
            sweep_batch_size: 100,
            similarity_threshold: 0.85,
        };

        let r = run_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep must not error");
        assert_eq!(r.skipped, 1, "low-confidence op must be counted as skipped");
        assert_eq!(r.merges, 0);
        assert_eq!(r.updates, 0);
    }

    /// #2360: after a successful update op, verify DB state:
    /// consolidated message is persisted, source messages are marked consolidated=1.
    #[tokio::test]
    async fn run_consolidation_sweep_update_db_state() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let store = Arc::new(SqliteStore::new(":memory:").await.unwrap());
        let conv_id = store.create_conversation().await.unwrap();
        let m1 = store
            .save_message(conv_id, "user", "Alice uses Rust")
            .await
            .unwrap();
        let m2 = store
            .save_message(conv_id, "user", "Alice loves Rust")
            .await
            .unwrap();

        let new_content = "Alice uses and loves Rust";
        let update_json = format!(
            r#"{{"op":"update","target_id":{},"new_content":"{new_content}","additional_source_ids":[{}],"confidence":0.90}}"#,
            m1.0, m2.0
        );
        let mut mock = MockProvider::with_responses(vec![update_json]);
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);

        let config = ConsolidationConfig {
            enabled: true,
            confidence_threshold: 0.7,
            sweep_interval_secs: 300,
            sweep_batch_size: 100,
            similarity_threshold: 0.85,
        };

        let r = run_consolidation_sweep(&store, &provider, &config)
            .await
            .expect("sweep must not error");
        assert_eq!(r.updates, 1);

        // Verify the consolidated message exists in DB.
        let consol_rows: Vec<(String, i64)> = zeph_db::query_as(sql!(
            "SELECT content, consolidated FROM messages \
             WHERE consolidated = 1 AND content = ?"
        ))
        .bind(new_content)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(
            consol_rows.len(),
            1,
            "one consolidated message must be persisted"
        );

        // Verify source m2 is marked consolidated (m2 was the additional_source_id).
        let source_row: (i64,) =
            zeph_db::query_as(sql!("SELECT consolidated FROM messages WHERE id = ?"))
                .bind(m2)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(source_row.0, 1, "source m2 must be marked consolidated=1");
    }
}
