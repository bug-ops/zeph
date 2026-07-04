// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
#[allow(unused_imports)]
use zeph_db::sql;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::Role;

use crate::embedding_store::SearchFilter;
use crate::store::SqliteStore;
use crate::token_counter::TokenCounter;
use crate::types::ConversationId;

use super::super::*;
use super::test_semantic_memory;

#[tokio::test]
async fn recall_returns_empty_without_qdrant() {
    let memory = test_semantic_memory(true).await;

    let recalled = memory.recall("test", 5, None).await.unwrap();
    assert!(recalled.is_empty());
}

#[tokio::test]
async fn recall_returns_empty_when_embeddings_not_supported() {
    let memory = test_semantic_memory(false).await;

    let recalled = memory.recall("test", 5, None).await.unwrap();
    assert!(recalled.is_empty());
}

#[tokio::test]
async fn test_semantic_memory_sqlite_remember_recall_roundtrip() {
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    let provider = AnyProvider::Mock(mock);

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let qdrant = Some(Arc::new(
        crate::embedding_store::EmbeddingStore::new_sqlite(pool),
    ));

    let memory = SemanticMemory {
        sqlite,
        qdrant,
        provider,
        embed_provider: None,
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay: super::super::TemporalDecay::Disabled,
        temporal_decay_half_life_days: 30,
        mmr_reranking: super::super::MmrReranking::Disabled,
        mmr_lambda: 0.7,
        importance_scoring: super::super::ImportanceScoring::Disabled,
        importance_weight: 0.15,
        token_counter: Arc::new(TokenCounter::new()),
        graph_store: None,
        experience: None,
        community_detection_failures: Arc::new(AtomicU64::new(0)),
        graph_extraction_count: Arc::new(AtomicU64::new(0)),
        graph_extraction_failures: Arc::new(AtomicU64::new(0)),
        last_qdrant_warn: Arc::new(AtomicU64::new(0)),
        tier_boost_semantic: 1.3,
        admission_control: None,
        quality_gate: None,
        key_facts_dedup_threshold: 0.95,
        embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
        retrieval_depth: 0,
        search_prompt_template: String::new(),
        depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reasoning: None,
        query_bias_correction: super::super::QueryBiasCorrection::Disabled,
        query_bias_profile_weight: 0.25,
        profile_centroid: tokio::sync::RwLock::new(None),
        profile_centroid_ttl_secs: 300,
        hebbian_reinforcement: super::super::HebbianReinforcement::Disabled,
        hebbian_lr: 0.1,
        hebbian_spread: crate::HelaSpreadRuntime::default(),
        retrieval_failure_logger: None,
        summarization_llm_timeout_secs: 60,
        query_sensitive_cost: false,
        five_signal: None,
        embed_timeout: std::time::Duration::from_secs(5),
        graph_cancel: std::sync::Mutex::new(Vec::new()),
    };

    let cid = memory.sqlite().create_conversation().await.unwrap();

    let id1 = memory
        .remember(cid, "user", "rust async programming", None)
        .await
        .unwrap();
    let id2 = memory
        .remember(cid, "assistant", "use tokio for async", None)
        .await
        .unwrap();
    assert!(id1 < id2);

    let recalled = memory.recall("rust", 5, None).await.unwrap();
    assert!(
        !recalled.is_empty(),
        "recall must return at least one result"
    );

    let history = memory.sqlite().load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "rust async programming");
}

#[tokio::test]
async fn embed_missing_without_embedding_support_returns_zero() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .sqlite()
        .save_message(cid, "user", "test message")
        .await
        .unwrap();

    let count = memory.embed_missing(None).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn recall_empty_without_qdrant_regardless_of_filter() {
    let memory = test_semantic_memory(true).await;
    let filter = SearchFilter {
        category: None,
        conversation_id: Some(ConversationId(1)),
        role: None,
    };
    let recalled = memory.recall("query", 10, Some(filter)).await.unwrap();
    assert!(recalled.is_empty());
}

#[tokio::test]
async fn recall_fts5_fallback_without_qdrant() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "rust programming guide", None)
        .await
        .unwrap();
    memory
        .remember(cid, "assistant", "python tutorial", None)
        .await
        .unwrap();
    memory
        .remember(cid, "user", "advanced rust patterns", None)
        .await
        .unwrap();

    let recalled = memory.recall("rust", 5, None).await.unwrap();
    assert_eq!(recalled.len(), 2);
    assert!(recalled[0].score >= recalled[1].score);
}

#[tokio::test]
async fn recall_fts5_fallback_with_filter() {
    let memory = test_semantic_memory(false).await;
    let cid1 = memory.sqlite.create_conversation().await.unwrap();
    let cid2 = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid1, "user", "hello world", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid2, "user", "hello universe", None)
        .await
        .unwrap();

    let filter = SearchFilter {
        category: None,
        conversation_id: Some(cid1),
        role: None,
    };
    let recalled = memory.recall("hello", 5, Some(filter)).await.unwrap();
    assert_eq!(recalled.len(), 1);
}

#[tokio::test]
async fn recall_fts5_no_matches_returns_empty() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "hello world", None)
        .await
        .unwrap()
        .unwrap();

    let recalled = memory.recall("nonexistent", 5, None).await.unwrap();
    assert!(recalled.is_empty());
}

#[tokio::test]
async fn recall_fts5_respects_limit() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    for i in 0..10 {
        memory
            .remember(cid, "user", &format!("test message number {i}"), None)
            .await
            .unwrap();
    }

    let recalled = memory.recall("test", 3, None).await.unwrap();
    assert_eq!(recalled.len(), 3);
}

#[tokio::test]
async fn recall_routed_keyword_route_returns_fts5_results() {
    use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "rust programming guide", None)
        .await
        .unwrap();
    memory
        .remember(cid, "assistant", "python tutorial", None)
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(router.route("rust_guide"), MemoryRoute::Keyword);

    let recalled = memory
        .recall_routed("rust_guide", 5, None, &router, None)
        .await
        .unwrap();
    assert!(recalled.len() <= 2);
}

#[tokio::test]
async fn recall_routed_semantic_route_without_qdrant_returns_empty_vectors() {
    use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "how does the agent loop work", None)
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(
        router.route("how does the agent loop work"),
        MemoryRoute::Semantic
    );

    let recalled = memory
        .recall_routed("how does the agent loop work", 5, None, &router, None)
        .await
        .unwrap();
    assert!(recalled.is_empty(), "no Qdrant → empty semantic recall");
}

#[tokio::test]
async fn recall_routed_hybrid_route_falls_back_to_fts5_on_no_qdrant() {
    use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "context window token budget", None)
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(
        router.route("context window token budget"),
        MemoryRoute::Hybrid
    );

    let recalled = memory
        .recall_routed("context window token budget", 5, None, &router, None)
        .await
        .unwrap();
    assert!(!recalled.is_empty(), "FTS5 should find the stored message");
}

#[tokio::test]
async fn recall_routed_episodic_route_no_time_range() {
    use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    // "when did" routes to Episodic and resolve_temporal_range returns None (no specific range),
    // so the dispatch uses plain FTS5 without a time filter — allowing recently stored messages
    // to be found in tests without time-zone or exact-timestamp dependencies.
    memory
        .remember(cid, "user", "we should discuss rust ownership", None)
        .await
        .unwrap();
    memory
        .remember(cid, "assistant", "python tutorial instead", None)
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(
        router.route("when did we discuss rust ownership"),
        MemoryRoute::Episodic
    );

    // Episodic dispatch pipeline:
    // 1. router returns Episodic
    // 2. strip_temporal_keywords strips "when did" → cleaned = "we discuss rust ownership"
    // 3. resolve_temporal_range returns None → falls back to recall_fts5_raw
    // 4. FTS5 finds the stored message by "rust" keyword
    let recalled = memory
        .recall_routed(
            "when did we discuss rust ownership",
            5,
            Some(SearchFilter {
                category: None,
                conversation_id: Some(cid),
                role: None,
            }),
            &router,
            None,
        )
        .await
        .unwrap();

    assert!(
        !recalled.is_empty(),
        "Episodic dispatch must find messages matching the stripped query"
    );
    assert!(
        recalled[0].message.content.contains("rust"),
        "first result must contain 'rust'"
    );
}

#[tokio::test]
async fn recall_routed_episodic_all_temporal_stripped_falls_back_to_original() {
    use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    // "last time" routes to Episodic, strip_temporal_keywords removes it leaving "".
    // recall_routed must fall back to the original query for FTS5 search.
    // resolve_temporal_range("last time") returns None so no time filter is applied,
    // allowing the recently stored message to be found.
    memory
        .remember(
            cid,
            "user",
            "last time we deployed the service it broke",
            None,
        )
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(router.route("last time"), MemoryRoute::Episodic);

    // strip_temporal_keywords("last time") → "" → fallback to original "last time" for FTS5.
    // The stored message contains "last time" so it must be found.
    let recalled = memory
        .recall_routed(
            "last time",
            5,
            Some(SearchFilter {
                category: None,
                conversation_id: Some(cid),
                role: None,
            }),
            &router,
            None,
        )
        .await
        .unwrap();
    assert!(
        !recalled.is_empty(),
        "fallback to original query must find the message containing 'last time'"
    );
}

// ── importance scoring tests (#2021) ──────────────────────────────────────

#[tokio::test]
async fn recall_importance_enabled_blends_score() {
    // Build a memory with importance_enabled = true and no Qdrant (pure FTS5).
    // A marker message should be boosted relative to a plain message.
    let memory = {
        let mut m = test_semantic_memory(false).await;
        m.importance_scoring = super::ImportanceScoring::Enabled;
        m.importance_weight = 0.20;
        m
    };

    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "remember: the API key rotates weekly", None)
        .await
        .unwrap();
    memory
        .remember(cid, "user", "API key info", None)
        .await
        .unwrap()
        .unwrap();

    let recalled = memory.recall("API key", 5, None).await.unwrap();
    assert!(
        recalled.len() >= 2,
        "both messages must be recalled, got {}",
        recalled.len()
    );

    // The marker message must outrank the plain one.
    let marker_rank = recalled
        .iter()
        .position(|r| r.message.content.contains("remember:"))
        .expect("marker message missing from recall results");
    assert_eq!(
        marker_rank, 0,
        "marker message must rank first when importance is enabled"
    );
}

#[tokio::test]
async fn recall_importance_disabled_no_blending() {
    // importance_enabled = false → scores must equal plain FTS5 weighted scores (no boost).
    // We verify the feature gate: turning it off must still work without panics.
    let memory = test_semantic_memory(false).await;

    let cid = memory.sqlite.create_conversation().await.unwrap();
    memory
        .remember(cid, "user", "remember: the API key rotates weekly", None)
        .await
        .unwrap();
    memory
        .remember(cid, "user", "API key info", None)
        .await
        .unwrap()
        .unwrap();

    let recalled = memory.recall("API key", 5, None).await.unwrap();
    // Must return results without panicking.
    assert!(!recalled.is_empty());
}

#[tokio::test]
async fn batch_increment_access_count_empty_vec_noop() {
    let memory = test_semantic_memory(false).await;
    // Recall on empty DB returns empty → access count increment is skipped.
    // No panic, no SQL error.
    let recalled = memory.recall("anything", 5, None).await.unwrap();
    assert!(recalled.is_empty());
}

#[tokio::test]
async fn recall_access_count_incremented_after_recall() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();
    let id = memory
        .remember(cid, "user", "rust async patterns", None)
        .await
        .unwrap();

    let before: (i64,) = zeph_db::query_as(sql!("SELECT access_count FROM messages WHERE id = ?"))
        .bind(id)
        .fetch_one(memory.sqlite.pool())
        .await
        .unwrap();
    assert_eq!(before.0, 0, "access_count must start at 0");

    let recalled = memory.recall("rust", 5, None).await.unwrap();
    assert!(!recalled.is_empty());

    let after: (i64,) = zeph_db::query_as(sql!("SELECT access_count FROM messages WHERE id = ?"))
        .bind(id)
        .fetch_one(memory.sqlite.pool())
        .await
        .unwrap();
    assert_eq!(after.0, 1, "access_count must be incremented after recall");
}

#[test]
fn recalled_message_debug() {
    use zeph_llm::provider::{Message, MessageMetadata};
    let recalled = RecalledMessage {
        message: Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        score: 0.95,
    };
    let dbg = format!("{recalled:?}");
    assert!(dbg.contains("RecalledMessage"));
    assert!(dbg.contains("0.95"));
}

// ── A-MAC admission control integration tests (#2317) ────────────────────────

fn make_always_reject_admission() -> crate::admission::AdmissionControl {
    // Threshold of 1.1 → no message can ever score above it → always rejected.
    let weights = crate::admission::AdmissionWeights {
        future_utility: 0.20,
        factual_confidence: 0.20,
        semantic_novelty: 0.20,
        temporal_recency: 0.20,
        content_type_prior: 0.20,
        goal_utility: 0.0,
    };
    crate::admission::AdmissionControl::new(1.1, 0.0, weights)
}

fn make_always_admit_admission() -> crate::admission::AdmissionControl {
    // Threshold of 0.0 → every message is admitted.
    let weights = crate::admission::AdmissionWeights {
        future_utility: 0.20,
        factual_confidence: 0.20,
        semantic_novelty: 0.20,
        temporal_recency: 0.20,
        content_type_prior: 0.20,
        goal_utility: 0.0,
    };
    crate::admission::AdmissionControl::new(0.0, 0.0, weights)
}

#[tokio::test]
async fn remember_returns_none_when_admission_rejects() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_reject_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember(cid, "user", "this message will be rejected", None)
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "remember() must return None when A-MAC rejects"
    );

    // SQLite must have no messages (rejected = not persisted).
    let history = memory.sqlite.load_history(cid, 50).await.unwrap();
    assert!(
        history.is_empty(),
        "rejected messages must not be persisted"
    );
}

#[tokio::test]
async fn remember_returns_some_when_admission_admits() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_admit_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember(cid, "user", "important factual content", None)
        .await
        .unwrap();
    assert!(
        result.is_some(),
        "remember() must return Some(id) when A-MAC admits"
    );

    let history = memory.sqlite.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 1, "admitted message must be persisted");
}

#[tokio::test]
async fn remember_with_parts_returns_none_when_admission_rejects() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_reject_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let (opt_id, stored) = memory
        .remember_with_parts(cid, "assistant", "rejected content", "[]", None)
        .await
        .unwrap();
    assert!(
        opt_id.is_none(),
        "remember_with_parts must return None when rejected"
    );
    assert!(!stored, "embedding_stored must be false when rejected");
}

#[tokio::test]
async fn remember_with_parts_returns_some_when_admission_admits() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_admit_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let (opt_id, _stored) = memory
        .remember_with_parts(cid, "user", "admitted content", "[]", None)
        .await
        .unwrap();
    assert!(
        opt_id.is_some(),
        "remember_with_parts must return Some(id) when admitted"
    );
}

#[tokio::test]
async fn admission_without_dedicated_provider_uses_embed_provider() {
    // Regression test for #3158: AdmissionControl with no dedicated provider (provider = None)
    // must succeed using the embed provider from SemanticMemory rather than panicking or
    // falling back to the main 1536-dim provider.
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_admit_admission());
    // make_always_admit_admission() creates AdmissionControl::new(...) with NO .with_provider()
    // call, which is exactly the unconfigured path fixed in src/bootstrap/mod.rs.

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember(
            cid,
            "user",
            "content evaluated without dedicated admission provider",
            None,
        )
        .await
        .unwrap();
    assert!(
        result.is_some(),
        "admission without dedicated provider must admit when threshold=0.0"
    );
}

// ── A-MAC admission training data collection tests (#5555) ───────────────────
//
// Regression coverage for issue #5555: `record_admission_sample` (the private helper
// wired into all 4 `remember*` methods) had no test asserting it actually writes to
// `admission_training_data`, nor that `message_id`/`was_admitted` are correct for each
// of the three possible outcomes. The existing A-MAC tests above only assert the
// `remember()` return value and `messages` table state — they would not catch
// `record_admission_sample` silently never being called, or being called with the
// wrong `message_id`.

#[tokio::test]
async fn rejected_by_amac_records_training_sample_with_no_message_id() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_reject_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember(cid, "user", "this message will be rejected", None)
        .await
        .unwrap();
    assert!(result.is_none());

    assert_eq!(
        memory.sqlite.count_training_records().await.unwrap(),
        1,
        "A-MAC rejection must still be recorded as a training sample"
    );
    let batch = memory.sqlite.get_training_batch(10).await.unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch[0].message_id, None,
        "rejected message was never persisted, so message_id must be None"
    );
    assert!(
        !batch[0].was_admitted,
        "training record must reflect the rejection"
    );
}

#[tokio::test]
async fn admitted_and_persisted_records_training_sample_with_message_id() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_admit_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let message_id = memory
        .remember(cid, "user", "important factual content", None)
        .await
        .unwrap()
        .expect("admitted message must return Some(id)");

    assert_eq!(memory.sqlite.count_training_records().await.unwrap(), 1);
    let batch = memory.sqlite.get_training_batch(10).await.unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch[0].message_id,
        Some(message_id.0),
        "admitted-and-persisted training record must carry the real message_id"
    );
    assert!(
        batch[0].was_admitted,
        "training record must reflect the admission"
    );
}

#[tokio::test]
async fn admitted_then_quality_gate_rejected_records_training_sample_with_no_message_id() {
    // Quality gate config matching `gate_rejects_pronoun_only_at_low_threshold` in
    // quality_gate.rs, which reliably rejects pronoun-heavy content as IncompleteReference.
    let gate_config = crate::quality_gate::QualityGateConfig {
        enabled: true,
        threshold: 0.75,
        reference_completeness_weight: 0.9,
        information_value_weight: 0.05,
        contradiction_weight: 0.05,
        ..crate::quality_gate::QualityGateConfig::default()
    };
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_admit_admission())
        .with_quality_gate(Arc::new(crate::quality_gate::QualityGate::new(gate_config)));

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember(cid, "user", "yeah he confirmed it they said so", None)
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "quality gate must reject this pronoun-heavy content after A-MAC admits it"
    );

    assert_eq!(
        memory.sqlite.count_training_records().await.unwrap(),
        1,
        "A-MAC-admitted-but-quality-gate-rejected message must still be recorded, to \
         avoid survivorship bias in the training set"
    );
    let batch = memory.sqlite.get_training_batch(10).await.unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch[0].message_id, None,
        "message was never persisted (quality gate rejected it after admission), \
         so message_id must be None"
    );
    assert!(
        batch[0].was_admitted,
        "was_admitted reflects the A-MAC decision, not the downstream quality-gate outcome"
    );
}

// ── #5569 DRY refactor coverage: run_admission_gate / run_quality_gate /
//    record_admission_sample_opt shared by all 4 `remember*` variants ───────────

#[tokio::test]
async fn remember_tool_output_returns_none_when_admission_rejects() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_reject_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let (opt_id, stored) = memory
        .remember_tool_output(
            cid,
            "assistant",
            "rejected tool output",
            "[]",
            EmbedContext::default(),
        )
        .await
        .unwrap();
    assert!(
        opt_id.is_none(),
        "remember_tool_output must return None when A-MAC rejects"
    );
    assert!(!stored, "embedding_stored must be false when rejected");
    assert_eq!(
        memory.sqlite.count_training_records().await.unwrap(),
        1,
        "rejection must still record exactly one training sample"
    );
}

#[tokio::test]
async fn remember_categorized_returns_none_when_admission_rejects() {
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_reject_admission());

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember_categorized(cid, "user", "rejected categorized content", None, None)
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "remember_categorized must return None when A-MAC rejects"
    );
    assert_eq!(
        memory.sqlite.count_training_records().await.unwrap(),
        1,
        "rejection must still record exactly one training sample"
    );
}

#[tokio::test]
async fn remember_tool_output_skips_quality_gate() {
    // Same gate config as `admitted_then_quality_gate_rejected_records_training_sample_with_no_message_id`,
    // which reliably rejects this exact pronoun-heavy content for `remember()`. Tool output
    // must NOT go through the quality gate, so it must be persisted here instead.
    let gate_config = crate::quality_gate::QualityGateConfig {
        enabled: true,
        threshold: 0.75,
        reference_completeness_weight: 0.9,
        information_value_weight: 0.05,
        contradiction_weight: 0.05,
        ..crate::quality_gate::QualityGateConfig::default()
    };
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_admit_admission())
        .with_quality_gate(Arc::new(crate::quality_gate::QualityGate::new(gate_config)));

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let (opt_id, _stored) = memory
        .remember_tool_output(
            cid,
            "assistant",
            "yeah he confirmed it they said so",
            "[]",
            EmbedContext::default(),
        )
        .await
        .unwrap();
    assert!(
        opt_id.is_some(),
        "remember_tool_output must skip the quality gate and persist admitted content"
    );
}

#[tokio::test]
async fn remember_categorized_skips_quality_gate() {
    let gate_config = crate::quality_gate::QualityGateConfig {
        enabled: true,
        threshold: 0.75,
        reference_completeness_weight: 0.9,
        information_value_weight: 0.05,
        contradiction_weight: 0.05,
        ..crate::quality_gate::QualityGateConfig::default()
    };
    let memory = test_semantic_memory(false)
        .await
        .with_admission_control(make_always_admit_admission())
        .with_quality_gate(Arc::new(crate::quality_gate::QualityGate::new(gate_config)));

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember_categorized(cid, "user", "yeah he confirmed it they said so", None, None)
        .await
        .unwrap();
    assert!(
        result.is_some(),
        "remember_categorized must skip the quality gate and persist admitted content"
    );
}

#[tokio::test]
async fn no_admission_control_configured_proceeds_without_recording_sample() {
    // test_semantic_memory() leaves admission_control = None, so run_admission_gate must
    // return Proceed(None) unconditionally and no training sample should ever be recorded.
    let memory = test_semantic_memory(false).await;

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let result = memory
        .remember(cid, "user", "no admission control configured", None)
        .await
        .unwrap();
    assert!(
        result.is_some(),
        "message must be persisted when no admission control is configured"
    );
    assert_eq!(
        memory.sqlite.count_training_records().await.unwrap(),
        0,
        "no training sample should be recorded without an AdmissionDecision"
    );
}

// ── effective_depth tests (MM-F1, #3340) ──────────────────────────────────────

#[tokio::test]
async fn effective_depth_legacy_sentinel_returns_limit_times_two() {
    let memory = test_semantic_memory(false).await;
    // depth = 0 (default) → legacy behavior
    assert_eq!(memory.effective_depth(10), 20);
    assert_eq!(memory.effective_depth(5), 10);
}

#[tokio::test]
async fn effective_depth_configured_returns_exact_value_when_ge_limit() {
    let memory = test_semantic_memory(false)
        .await
        .with_retrieval_options(15, "");
    assert_eq!(memory.effective_depth(10), 15);
}

#[tokio::test]
async fn effective_depth_configured_returns_exact_value_when_below_limit() {
    // depth < limit: WARN fires once, value returned unchanged
    let memory = test_semantic_memory(false)
        .await
        .with_retrieval_options(5, "");
    assert_eq!(memory.effective_depth(10), 5);
}

// ── apply_search_prompt tests (MM-F2, #3340) ──────────────────────────────────

#[test]
fn apply_search_prompt_empty_template_returns_raw_query() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let memory = rt.block_on(test_semantic_memory(false));
    assert_eq!(memory.apply_search_prompt("hello"), "hello");
}

#[test]
fn apply_search_prompt_substitutes_placeholder() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let memory = rt
        .block_on(test_semantic_memory(false))
        .with_retrieval_options(0, "query: {query}");
    assert_eq!(
        memory.apply_search_prompt("rust async"),
        "query: rust async"
    );
}

#[test]
fn apply_search_prompt_substitutes_all_occurrences() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let memory = rt
        .block_on(test_semantic_memory(false))
        .with_retrieval_options(0, "{query}/{query}");
    assert_eq!(memory.apply_search_prompt("foo"), "foo/foo");
}

#[test]
fn apply_search_prompt_missing_placeholder_returns_raw_query() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let memory = rt
        .block_on(test_semantic_memory(false))
        .with_retrieval_options(0, "no placeholder here");
    // warn fires, raw query returned
    assert_eq!(memory.apply_search_prompt("hello"), "hello");
}
