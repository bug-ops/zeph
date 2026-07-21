// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;

use crate::db_vector_store::DbVectorStore;
use crate::store::SqliteStore;
use crate::token_counter::TokenCounter;
use crate::types::{ConversationId, MessageId};
use crate::vector_store::{
    BoxFuture, ScoredVectorPoint, ScrollResult, ScrollWithIdsResult, VectorFilter, VectorPoint,
    VectorStore, VectorStoreError,
};

use super::super::*;
use super::test_semantic_memory;

/// A `VectorStore` wrapper that delegates all operations to an inner store except `search`,
/// which always returns a `VectorStoreError::Search` error.  Used to test the fail-open path
/// in `store_key_fact_if_unique`.
struct FailingSearchStore(DbVectorStore);

impl VectorStore for FailingSearchStore {
    fn ensure_collection(
        &self,
        collection: &str,
        vector_size: u64,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        self.0.ensure_collection(collection, vector_size)
    }

    fn collection_exists(&self, collection: &str) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
        self.0.collection_exists(collection)
    }

    fn delete_collection(&self, collection: &str) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        self.0.delete_collection(collection)
    }

    fn upsert(
        &self,
        collection: &str,
        points: Vec<VectorPoint>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        self.0.upsert(collection, points)
    }

    fn search_clamped(
        &self,
        _collection: &str,
        _vector: Vec<f32>,
        _limit: u64,
        _filter: Option<VectorFilter>,
    ) -> BoxFuture<'_, Result<Vec<ScoredVectorPoint>, VectorStoreError>> {
        Box::pin(async { Err(VectorStoreError::Search("injected search error".into())) })
    }

    fn delete_by_ids(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        self.0.delete_by_ids(collection, ids)
    }

    fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollResult, VectorStoreError>> {
        self.0.scroll_all(collection, key_field)
    }

    fn scroll_all_with_point_ids(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollWithIdsResult, VectorStoreError>> {
        self.0.scroll_all_with_point_ids(collection, key_field)
    }

    fn health_check(&self) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
        self.0.health_check()
    }

    fn create_keyword_indexes(
        &self,
        collection: &str,
        fields: &[&str],
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        self.0.create_keyword_indexes(collection, fields)
    }
}

#[tokio::test]
async fn unsummarized_count_decreases_after_summary() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..10 {
        memory
            .remember(cid, "user", &format!("msg{i}"), None)
            .await
            .unwrap();
    }
    assert_eq!(memory.unsummarized_message_count(cid).await.unwrap(), 10);

    memory.summarize(cid, 5).await.unwrap();

    assert!(memory.unsummarized_message_count(cid).await.unwrap() < 10);
    assert_eq!(memory.message_count(cid).await.unwrap(), 10);
}

#[tokio::test]
async fn load_summaries_empty() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let summaries = memory.load_summaries(cid).await.unwrap();
    assert!(summaries.is_empty());
}

#[tokio::test]
async fn load_summaries_ordered() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let msg_id1 = memory
        .remember(cid, "user", "m1", None)
        .await
        .unwrap()
        .unwrap();
    let msg_id2 = memory
        .remember(cid, "assistant", "m2", None)
        .await
        .unwrap()
        .unwrap();
    let msg_id3 = memory
        .remember(cid, "user", "m3", None)
        .await
        .unwrap()
        .unwrap();

    let s1 = memory
        .sqlite()
        .save_summary(cid, "summary1", Some(msg_id1), Some(msg_id2), 3)
        .await
        .unwrap();
    let s2 = memory
        .sqlite()
        .save_summary(cid, "summary2", Some(msg_id2), Some(msg_id3), 3)
        .await
        .unwrap();

    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, s1);
    assert_eq!(summaries[0].content, "summary1");
    assert_eq!(summaries[1].id, s2);
    assert_eq!(summaries[1].content, "summary2");
}

#[tokio::test]
async fn summarize_below_threshold() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "hello", None)
        .await
        .unwrap()
        .unwrap();

    let result = memory.summarize(cid, 10).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn summarize_stores_summary() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..5 {
        memory
            .remember(cid, "user", &format!("message {i}"), None)
            .await
            .unwrap();
    }

    let outcome = memory.summarize(cid, 3).await.unwrap();
    assert!(outcome.is_some());
    let outcome = outcome.unwrap();
    assert_eq!(
        outcome.messages_folded, 3,
        "must fold exactly message_count messages"
    );

    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, outcome.summary_id);
    assert!(!summaries[0].content.is_empty());
}

#[tokio::test]
async fn summarize_respects_previous_summaries() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..10 {
        memory
            .remember(cid, "user", &format!("message {i}"), None)
            .await
            .unwrap();
    }

    let s1 = memory.summarize(cid, 3).await.unwrap();
    assert!(s1.is_some());

    let s2 = memory.summarize(cid, 3).await.unwrap();
    assert!(s2.is_some());

    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 2);
    assert!(summaries[0].last_message_id < summaries[1].first_message_id);
}

#[tokio::test]
async fn summarize_exact_threshold_returns_none() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..3 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    let result = memory.summarize(cid, 3).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn summarize_one_above_threshold_produces_summary() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..4 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    let result = memory.summarize(cid, 3).await.unwrap();
    assert!(result.is_some());
}

#[tokio::test]
async fn summary_fields_populated() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..5 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    memory.summarize(cid, 3).await.unwrap();
    let summaries = memory.load_summaries(cid).await.unwrap();
    let s = &summaries[0];

    assert_eq!(s.conversation_id, cid);
    assert!(s.first_message_id > Some(MessageId(0)));
    assert!(s.last_message_id >= s.first_message_id);
    assert!(s.token_estimate >= 0);
    assert!(!s.content.is_empty());
}

#[tokio::test]
async fn summarize_empty_messages_range_returns_none() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..6 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    memory.summarize(cid, 3).await.unwrap();
    memory.summarize(cid, 3).await.unwrap();

    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 2);
}

#[tokio::test]
async fn summarize_token_estimate_populated() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..5 {
        memory
            .remember(cid, "user", &format!("message {i}"), None)
            .await
            .unwrap();
    }

    memory.summarize(cid, 3).await.unwrap();
    let summaries = memory.load_summaries(cid).await.unwrap();
    let token_est = summaries[0].token_estimate;
    assert!(token_est > 0);
}

#[tokio::test]
async fn summarize_fails_when_provider_chat_fails() {
    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let provider = AnyProvider::Ollama(zeph_llm::ollama::OllamaProvider::new(
        "http://127.0.0.1:1",
        "test".into(),
        "embed".into(),
    ));
    let memory = super::super::SemanticMemory {
        sqlite,
        qdrant: None,
        provider,
        embed_provider: None,
        embedding_model: "test".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay: TemporalDecay::Disabled,
        temporal_decay_half_life_days: 30,
        mmr_reranking: MmrReranking::Disabled,
        mmr_lambda: 0.7,
        importance_scoring: ImportanceScoring::Disabled,
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
        query_bias_correction: QueryBiasCorrection::Disabled,
        query_bias_profile_weight: 0.25,
        profile_centroid: tokio::sync::RwLock::new(None),
        profile_centroid_ttl_secs: 300,
        hebbian_reinforcement: HebbianReinforcement::Disabled,
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

    for i in 0..5 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    let result = memory.summarize(cid, 3).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn summarize_message_range_bounds() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..8 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    let outcome = memory.summarize(cid, 4).await.unwrap().unwrap();
    assert_eq!(
        outcome.messages_folded, 4,
        "must fold exactly message_count messages"
    );
    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, outcome.summary_id);
    assert!(summaries[0].first_message_id >= Some(MessageId(1)));
    assert!(summaries[0].last_message_id >= summaries[0].first_message_id);
}

// ── MemoryFacade::compact messages_compacted accuracy (#5533) ──────────────────

#[tokio::test]
async fn compact_reports_actual_messages_folded_not_total_message_count() {
    use crate::facade::{CompactionContext, MemoryFacade};

    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..20 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    // token_budget=40 -> target_msgs = 40/4 = 10, so summarize() folds only 10 of the 20
    // unsummarized messages (capped by the range LIMIT), not the full conversation.
    let result = memory
        .compact(&CompactionContext {
            conversation_id: cid,
            token_budget: 40,
        })
        .await
        .unwrap();

    assert_eq!(
        result.messages_compacted, 10,
        "messages_compacted must reflect what summarize() actually folded, not the total \
         message count fetched before summarization"
    );
}

#[tokio::test]
async fn compact_reports_zero_when_summarize_is_noop() {
    use crate::facade::{CompactionContext, MemoryFacade};

    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "only one message", None)
        .await
        .unwrap();

    // token_budget=4096 -> target_msgs=1024, far above the single message present, so
    // summarize() is a no-op (total <= message_count) and returns None.
    let result = memory
        .compact(&CompactionContext {
            conversation_id: cid,
            token_budget: 4096,
        })
        .await
        .unwrap();

    assert_eq!(
        result.messages_compacted, 0,
        "messages_compacted must be 0 when summarize() is a no-op"
    );
}

#[tokio::test]
async fn summarize_fallback_to_plain_text_when_structured_fails() {
    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let mut mock = MockProvider::default();
    mock.default_response = "plain text summary".into();
    let provider = AnyProvider::Mock(mock);

    let memory = super::super::SemanticMemory {
        sqlite,
        qdrant: None,
        provider,
        embed_provider: None,
        embedding_model: "test".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay: TemporalDecay::Disabled,
        temporal_decay_half_life_days: 30,
        mmr_reranking: MmrReranking::Disabled,
        mmr_lambda: 0.7,
        importance_scoring: ImportanceScoring::Disabled,
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
        query_bias_correction: QueryBiasCorrection::Disabled,
        query_bias_profile_weight: 0.25,
        profile_centroid: tokio::sync::RwLock::new(None),
        profile_centroid_ttl_secs: 300,
        hebbian_reinforcement: HebbianReinforcement::Disabled,
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
    for i in 0..5 {
        memory
            .remember(cid, "user", &format!("msg {i}"), None)
            .await
            .unwrap();
    }

    let result = memory.summarize(cid, 3).await;
    assert!(result.is_ok());
    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert!(!summaries[0].content.is_empty());
}

#[test]
fn build_summarization_prompt_format() {
    let messages = vec![
        (MessageId(1), "user".into(), "Hello".into()),
        (MessageId(2), "assistant".into(), "Hi there".into()),
    ];
    let prompt = build_summarization_prompt(&messages);
    assert!(prompt.contains("user: Hello"));
    assert!(prompt.contains("assistant: Hi there"));
    assert!(prompt.contains("key_facts"));
}

#[test]
fn build_summarization_prompt_empty() {
    let messages: Vec<(MessageId, String, String)> = vec![];
    let prompt = build_summarization_prompt(&messages);
    assert!(prompt.contains("key_facts"));
}

#[test]
fn build_summarization_prompt_preserves_order() {
    let messages = vec![
        (MessageId(1), "user".into(), "first".into()),
        (MessageId(2), "assistant".into(), "second".into()),
        (MessageId(3), "user".into(), "third".into()),
    ];
    let prompt = build_summarization_prompt(&messages);
    let first_pos = prompt.find("user: first").unwrap();
    let second_pos = prompt.find("assistant: second").unwrap();
    let third_pos = prompt.find("user: third").unwrap();
    assert!(first_pos < second_pos);
    assert!(second_pos < third_pos);
}

#[test]
fn structured_summary_deserialize() {
    let json = r#"{"summary":"s","key_facts":["f1","f2"],"entities":["e1"]}"#;
    let ss: StructuredSummary = serde_json::from_str(json).unwrap();
    assert_eq!(ss.summary, "s");
    assert_eq!(ss.key_facts.len(), 2);
    assert_eq!(ss.entities.len(), 1);
}

#[test]
fn structured_summary_empty_facts() {
    let json = r#"{"summary":"s","key_facts":[],"entities":[]}"#;
    let ss: StructuredSummary = serde_json::from_str(json).unwrap();
    assert!(ss.key_facts.is_empty());
    assert!(ss.entities.is_empty());
}

#[test]
fn summary_clone() {
    let summary = Summary {
        id: 1,
        conversation_id: ConversationId(2),
        content: "test summary".into(),
        first_message_id: Some(MessageId(1)),
        last_message_id: Some(MessageId(5)),
        token_estimate: 10,
    };
    let cloned = summary.clone();
    assert_eq!(summary.id, cloned.id);
    assert_eq!(summary.content, cloned.content);
}

#[test]
fn summary_debug() {
    let summary = Summary {
        id: 1,
        conversation_id: ConversationId(2),
        content: "test".into(),
        first_message_id: Some(MessageId(1)),
        last_message_id: Some(MessageId(5)),
        token_estimate: 10,
    };
    let dbg = format!("{summary:?}");
    assert!(dbg.contains("Summary"));
}

#[tokio::test]
async fn search_key_facts_no_qdrant_empty() {
    let memory = test_semantic_memory(false).await;
    let facts = memory.search_key_facts("query", 5, None).await.unwrap();
    assert!(facts.is_empty());
}

#[tokio::test]
async fn load_summaries_nonexistent_conversation() {
    let memory = test_semantic_memory(false).await;
    let summaries = memory.load_summaries(ConversationId(999)).await.unwrap();
    assert!(summaries.is_empty());
}

async fn make_embed_memory_with_threshold(threshold: f32) -> super::super::SemanticMemory {
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    // Use a non-zero unit-ish vector so cosine similarity is well-defined (not 0/0).
    mock.embedding = {
        let mut v = vec![0.0f32; 384];
        v[0] = 1.0;
        v
    };
    let provider = AnyProvider::Mock(mock);

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let store = crate::embedding_store::EmbeddingStore::new_sqlite(pool);

    super::super::SemanticMemory {
        sqlite,
        qdrant: Some(Arc::new(store)),
        provider,
        embed_provider: None,
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay: TemporalDecay::Disabled,
        temporal_decay_half_life_days: 30,
        mmr_reranking: MmrReranking::Disabled,
        mmr_lambda: 0.7,
        importance_scoring: ImportanceScoring::Disabled,
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
        key_facts_dedup_threshold: threshold,
        embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
        retrieval_depth: 0,
        search_prompt_template: String::new(),
        depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reasoning: None,
        query_bias_correction: QueryBiasCorrection::Disabled,
        query_bias_profile_weight: 0.25,
        profile_centroid: tokio::sync::RwLock::new(None),
        profile_centroid_ttl_secs: 300,
        hebbian_reinforcement: HebbianReinforcement::Disabled,
        hebbian_lr: 0.1,
        hebbian_spread: crate::HelaSpreadRuntime::default(),
        retrieval_failure_logger: None,
        summarization_llm_timeout_secs: 60,
        query_sensitive_cost: false,
        five_signal: None,
        embed_timeout: std::time::Duration::from_secs(5),
        graph_cancel: std::sync::Mutex::new(Vec::new()),
    }
}

/// Like [`make_embed_memory_with_threshold`], but the returned memory's `EmbeddingStore` is
/// tagged with `db_instance_id` and backed by `shared_vector_pool` instead of its own private
/// `SqliteStore` pool — simulating a second physical database sharing one Qdrant instance with
/// whatever other store(s) were also built against `shared_vector_pool` (#5742).
///
/// The returned memory keeps its own private `SqliteStore` for conversation/message ids, so
/// calling `.sqlite().create_conversation()` on two memories built this way independently
/// starts each at `conversation_id = 1` — exactly the collision scenario the issue describes.
async fn make_embed_memory_with_db_instance(
    threshold: f32,
    db_instance_id: &str,
    shared_vector_pool: zeph_db::DbPool,
) -> super::super::SemanticMemory {
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    // Use a non-zero unit-ish vector so cosine similarity is well-defined (not 0/0).
    mock.embedding = {
        let mut v = vec![0.0f32; 384];
        v[0] = 1.0;
        v
    };
    let provider = AnyProvider::Mock(mock);

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let store = crate::embedding_store::EmbeddingStore::with_store(
        Box::new(DbVectorStore::new(shared_vector_pool)),
        pool,
    )
    .with_db_instance_id(db_instance_id);

    super::super::SemanticMemory {
        sqlite,
        qdrant: Some(Arc::new(store)),
        provider,
        embed_provider: None,
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay: TemporalDecay::Disabled,
        temporal_decay_half_life_days: 30,
        mmr_reranking: MmrReranking::Disabled,
        mmr_lambda: 0.7,
        importance_scoring: ImportanceScoring::Disabled,
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
        key_facts_dedup_threshold: threshold,
        embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
        retrieval_depth: 0,
        search_prompt_template: String::new(),
        depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reasoning: None,
        query_bias_correction: QueryBiasCorrection::Disabled,
        query_bias_profile_weight: 0.25,
        profile_centroid: tokio::sync::RwLock::new(None),
        profile_centroid_ttl_secs: 300,
        hebbian_reinforcement: HebbianReinforcement::Disabled,
        hebbian_lr: 0.1,
        hebbian_spread: crate::HelaSpreadRuntime::default(),
        retrieval_failure_logger: None,
        summarization_llm_timeout_secs: 60,
        query_sensitive_cost: false,
        five_signal: None,
        embed_timeout: std::time::Duration::from_secs(5),
        graph_cancel: std::sync::Mutex::new(Vec::new()),
    }
}

#[tokio::test]
async fn store_key_facts_first_fact_stored() {
    let memory = make_embed_memory_with_threshold(0.95).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .store_key_facts(cid, 1, &["unique fact".to_string()])
        .await;
    let results = memory
        .search_key_facts("unique fact", 5, Some(cid))
        .await
        .unwrap();
    assert!(!results.is_empty(), "first fact must be stored");
}

#[tokio::test]
async fn store_key_facts_duplicate_skipped_at_threshold() {
    // MockProvider always returns vec![0.0; 384], so cosine similarity between any two
    // embeddings is always 1.0.  With threshold=0.95, the second insert must be skipped.
    let memory = make_embed_memory_with_threshold(0.95).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .store_key_facts(cid, 1, &["fact A".to_string()])
        .await;
    memory
        .store_key_facts(cid, 2, &["fact A again".to_string()])
        .await;
    // Both embed to the same unit vector (v[0]=1.0); second should be deduplicated.
    let results = memory
        .search_key_facts("fact A", 10, Some(cid))
        .await
        .unwrap();
    assert_eq!(results.len(), 1, "duplicate fact must be skipped");
}

#[tokio::test]
async fn store_key_facts_dedup_scoped_per_conversation() {
    // S1 regression (#5732 follow-up): the near-duplicate dedup check must be scoped to the
    // same conversation as the read-side filter. Before the fix, dedup searched globally, so a
    // fact stored under conversation A would silently suppress storage of a near-identical fact
    // under conversation B — and B's conversation-scoped search_key_facts would then never see
    // its own fact at all.
    let memory = make_embed_memory_with_threshold(0.95).await;
    let conv_a = memory.sqlite().create_conversation().await.unwrap();
    let conv_b = memory.sqlite().create_conversation().await.unwrap();

    memory
        .store_key_facts(conv_a, 1, &["shared fact".to_string()])
        .await;
    // Same mock embedding vector as conv_a's fact, so a global dedup search would find it and
    // (incorrectly) skip storing this one under conv_b.
    memory
        .store_key_facts(conv_b, 2, &["shared fact again".to_string()])
        .await;

    let results_b = memory
        .search_key_facts("shared fact", 10, Some(conv_b))
        .await
        .unwrap();
    assert!(
        !results_b.is_empty(),
        "conversation B's own fact must not be suppressed by conversation A's near-duplicate"
    );
}

#[tokio::test]
async fn store_key_facts_dedup_not_falsely_matched_across_db_instances() {
    // #5742 regression: two independent databases (own conversation_id counters, each starting
    // at 1) share one backing vector store. Without db_instance_id in the dedup filter, db-b's
    // fact under conversation_id=1 would be (incorrectly) treated as a near-duplicate of db-a's
    // fact under the same conversation_id and silently skipped.
    let shared = SqliteStore::new(":memory:").await.unwrap();
    let shared_pool = shared.pool().clone();

    let memory_a = make_embed_memory_with_db_instance(0.95, "db-a", shared_pool.clone()).await;
    let memory_b = make_embed_memory_with_db_instance(0.95, "db-b", shared_pool.clone()).await;

    let cid_a = memory_a.sqlite().create_conversation().await.unwrap();
    let cid_b = memory_b.sqlite().create_conversation().await.unwrap();
    assert_eq!(
        cid_a, cid_b,
        "both databases must independently start at conversation_id=1"
    );

    memory_a
        .store_key_facts(cid_a, 1, &["shared fact".to_string()])
        .await;
    // Same mock embedding vector as db-a's fact and the same conversation_id — a dedup filter
    // that omits db_instance_id would find db-a's fact and (incorrectly) skip storing this one.
    memory_b
        .store_key_facts(cid_b, 2, &["shared fact again".to_string()])
        .await;

    let results_b = memory_b
        .search_key_facts("shared fact", 10, Some(cid_b))
        .await
        .unwrap();
    // Assert on the exact text (not just non-emptiness): db-b's search filter is itself scoped by
    // db_instance_id, so a leaked read of db-a's fact would also make `results_b` non-empty
    // without proving db-b's own insert actually happened.
    assert!(
        results_b.iter().any(|f| f.contains("shared fact again")),
        "db-b's own fact must not be suppressed by db-a's near-duplicate under the same conversation_id: {results_b:?}"
    );
}

#[tokio::test]
async fn store_key_facts_stored_when_threshold_above_one() {
    // With threshold > 1.0, cosine similarity can never reach it, so dedup never fires.
    // Both facts (same vector) must be stored.
    let memory = make_embed_memory_with_threshold(2.0).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .store_key_facts(cid, 1, &["fact C".to_string()])
        .await;
    memory
        .store_key_facts(cid, 2, &["fact C twin".to_string()])
        .await;
    let results = memory
        .search_key_facts("fact C", 10, Some(cid))
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        2,
        "both facts must be stored when threshold > 1.0"
    );
}

#[tokio::test]
async fn store_key_facts_fail_open_on_search_error() {
    // When search_collection returns an error, the dedup check is skipped and the fact
    // must still be stored (fail-open guarantee).
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    mock.embedding = {
        let mut v = vec![0.0f32; 384];
        v[0] = 1.0;
        v
    };
    let provider = AnyProvider::Mock(mock);

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let failing_store = FailingSearchStore(DbVectorStore::new(pool.clone()));
    let store = crate::embedding_store::EmbeddingStore::with_store(Box::new(failing_store), pool);

    let memory = super::super::SemanticMemory {
        sqlite,
        qdrant: Some(Arc::new(store)),
        provider,
        embed_provider: None,
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay: TemporalDecay::Disabled,
        temporal_decay_half_life_days: 30,
        mmr_reranking: MmrReranking::Disabled,
        mmr_lambda: 0.7,
        importance_scoring: ImportanceScoring::Disabled,
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
        query_bias_correction: QueryBiasCorrection::Disabled,
        query_bias_profile_weight: 0.25,
        profile_centroid: tokio::sync::RwLock::new(None),
        profile_centroid_ttl_secs: 300,
        hebbian_reinforcement: HebbianReinforcement::Disabled,
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

    // store_key_facts must complete without panicking even though every search call will fail.
    // This is the documented fail-open guarantee: a dedup-search error does not suppress
    // the insert, and the error is not propagated to the caller.
    memory
        .store_key_facts(cid, 1, &["fact stored despite search error".to_string()])
        .await;
    // Reaching this line confirms: no panic, no propagated error.
}

// ── Cross-conversation Key Facts isolation (#5732) ─────────────────────────────

#[tokio::test]
async fn search_key_facts_scoped_excludes_other_conversation_fact() {
    // threshold=2.0 disables dedup (cosine similarity can never reach it), so the store
    // is not the thing under test here — cross-conversation filtering is.
    let memory = make_embed_memory_with_threshold(2.0).await;
    let cid_a = memory.sqlite().create_conversation().await.unwrap();
    let cid_b = memory.sqlite().create_conversation().await.unwrap();

    memory
        .store_key_facts(cid_a, 1, &["Alice's favorite color is blue".to_string()])
        .await;

    let results_b = memory
        .search_key_facts("Alice's favorite color", 5, Some(cid_b))
        .await
        .unwrap();
    assert!(
        results_b.is_empty(),
        "a fact stored under conversation A must not leak into conversation B's scoped search"
    );

    let results_a = memory
        .search_key_facts("Alice's favorite color", 5, Some(cid_a))
        .await
        .unwrap();
    assert!(
        !results_a.is_empty(),
        "a fact stored under conversation A must be found when searching scoped to A"
    );
    assert!(results_a[0].contains("Alice's favorite color"));
}

#[tokio::test]
async fn search_key_facts_scoped_excludes_other_database_same_conversation_id() {
    // #5742 regression: two independent databases (own conversation_id counters, each starting
    // at 1) share one backing vector store, simulating two SQLite databases pointed at the same
    // Qdrant instance. Before the fix, a `conversation_id`-only filter would let db-b's fact leak
    // into db-a's scoped search and vice versa.
    let shared = SqliteStore::new(":memory:").await.unwrap();
    let shared_pool = shared.pool().clone();

    let memory_a = make_embed_memory_with_db_instance(2.0, "db-a", shared_pool.clone()).await;
    let memory_b = make_embed_memory_with_db_instance(2.0, "db-b", shared_pool.clone()).await;

    let cid_a = memory_a.sqlite().create_conversation().await.unwrap();
    let cid_b = memory_b.sqlite().create_conversation().await.unwrap();
    assert_eq!(
        cid_a, cid_b,
        "both databases must independently start at conversation_id=1"
    );

    memory_a
        .store_key_facts(cid_a, 1, &["Alice's favorite color is blue".to_string()])
        .await;
    memory_b
        .store_key_facts(cid_b, 1, &["Bob's favorite color is green".to_string()])
        .await;

    let results_a = memory_a
        .search_key_facts("favorite color", 5, Some(cid_a))
        .await
        .unwrap();
    assert_eq!(
        results_a.len(),
        1,
        "db-a's scoped search must not see db-b's fact even though both use conversation_id=1"
    );
    assert!(results_a[0].contains("Alice"));

    let results_b = memory_b
        .search_key_facts("favorite color", 5, Some(cid_b))
        .await
        .unwrap();
    assert_eq!(
        results_b.len(),
        1,
        "db-b's scoped search must not see db-a's fact even though both use conversation_id=1"
    );
    assert!(results_b[0].contains("Bob"));
}

#[tokio::test]
async fn search_key_facts_unscoped_still_sees_all_conversations() {
    // Passing `None` preserves the pre-fix, cross-conversation search behavior.
    let memory = make_embed_memory_with_threshold(2.0).await;
    let cid_a = memory.sqlite().create_conversation().await.unwrap();

    memory
        .store_key_facts(cid_a, 1, &["Bob's favorite language is Rust".to_string()])
        .await;

    let results = memory
        .search_key_facts("Bob's favorite language", 5, None)
        .await
        .unwrap();
    assert!(
        !results.is_empty(),
        "unscoped search (conversation_id = None) must still find facts from any conversation"
    );
}

#[tokio::test]
async fn search_key_facts_excludes_untagged_points_when_scoped() {
    // Simulates a point written before the #5732 fix (or a cross-conversation
    // episodic-consolidation fact promoted with no `conversation_id` tag): it must never
    // match a conversation-scoped search, but must still be visible to an unscoped search.
    let memory = make_embed_memory_with_threshold(2.0).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let qdrant = memory.qdrant.as_ref().expect("qdrant must be configured");
    let vector = {
        let mut v = vec![0.0f32; 384];
        v[0] = 1.0;
        v
    };
    qdrant
        .ensure_named_collection_for_vector(KEY_FACTS_COLLECTION, &vector)
        .await
        .unwrap();
    qdrant
        .store_to_collection(
            KEY_FACTS_COLLECTION,
            serde_json::json!({
                "fact_text": "untagged legacy fact",
                "source_summary_id": 1,
            }),
            vector,
        )
        .await
        .unwrap();

    let scoped = memory
        .search_key_facts("untagged legacy fact", 5, Some(cid))
        .await
        .unwrap();
    assert!(
        scoped.is_empty(),
        "a point with no conversation_id payload field must not match any conversation-scoped filter"
    );

    let unscoped = memory
        .search_key_facts("untagged legacy fact", 5, None)
        .await
        .unwrap();
    assert!(
        !unscoped.is_empty(),
        "a point with no conversation_id payload field must still be visible to an unscoped search"
    );
}

// ── is_policy_decision_fact ────────────────────────────────────────────────────

#[test]
fn policy_decision_fact_blocked_rejected() {
    use crate::semantic::summarization::is_policy_decision_fact;
    assert!(is_policy_decision_fact(
        "Reading the file was blocked by utility policy."
    ));
}

#[test]
fn policy_decision_fact_skipped_rejected() {
    use crate::semantic::summarization::is_policy_decision_fact;
    assert!(is_policy_decision_fact(
        "The tool call was skipped because access is restricted."
    ));
}

#[test]
fn policy_decision_fact_permission_denied_rejected() {
    use crate::semantic::summarization::is_policy_decision_fact;
    assert!(is_policy_decision_fact("permission denied for shell tool"));
}

#[test]
fn policy_decision_fact_cannot_access_rejected() {
    use crate::semantic::summarization::is_policy_decision_fact;
    assert!(is_policy_decision_fact(
        "Agent cannot access the filesystem path."
    ));
}

#[test]
fn policy_decision_fact_normal_fact_accepted() {
    use crate::semantic::summarization::is_policy_decision_fact;
    assert!(!is_policy_decision_fact(
        "The project uses Rust edition 2024."
    ));
}

#[test]
fn policy_decision_fact_case_insensitive() {
    use crate::semantic::summarization::is_policy_decision_fact;
    assert!(is_policy_decision_fact("Action BLOCKED by Security Policy"));
}
