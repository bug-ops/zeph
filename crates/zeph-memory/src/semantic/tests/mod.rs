// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod algorithms;
mod graph;
mod recall;
mod summarization;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use zeph_llm::LlmProvider;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::Role;

use crate::store::SqliteStore;
use crate::token_counter::TokenCounter;
use crate::types::{ConversationId, MessageId};

use super::*;

pub(super) fn test_provider() -> AnyProvider {
    AnyProvider::Mock(MockProvider::default())
}

pub(super) async fn test_semantic_memory(_supports_embeddings: bool) -> SemanticMemory {
    let provider = test_provider();
    let sqlite = SqliteStore::new(":memory:").await.unwrap();

    SemanticMemory {
        sqlite,
        qdrant: None,
        provider,
        embed_provider: None,
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay_enabled: false,
        temporal_decay_half_life_days: 30,
        mmr_enabled: false,
        mmr_lambda: 0.7,
        importance_enabled: false,
        importance_weight: 0.15,
        token_counter: Arc::new(TokenCounter::new()),
        graph_store: None,
        experience: None,
        reasoning: None,
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
        depth_below_limit_warned: Arc::new(AtomicBool::new(false)),
        missing_placeholder_warned: Arc::new(AtomicBool::new(false)),
    }
}

#[tokio::test]
async fn with_qdrant_ops_constructs_successfully() {
    let ops = crate::QdrantOps::new("http://127.0.0.1:1").unwrap();
    let provider = test_provider();
    let result =
        SemanticMemory::with_qdrant_ops(":memory:", ops, provider, "test-model", 0.7, 0.3, 1).await;
    assert!(
        result.is_ok(),
        "with_qdrant_ops must succeed (lazy TCP connect)"
    );
}

#[tokio::test]
async fn remember_saves_to_sqlite() {
    let memory = test_semantic_memory(false).await;

    let cid = memory.sqlite.create_conversation().await.unwrap();
    let msg_id = memory
        .remember(cid, "user", "hello", None)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(msg_id, MessageId(1));

    let history = memory.sqlite.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[0].content, "hello");
}

#[tokio::test]
async fn remember_with_parts_saves_parts_json() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    let parts_json =
        r#"[{"kind":"ToolOutput","tool_name":"shell","body":"hello","compacted_at":null}]"#;
    let (msg_id_opt, _embedding_stored) = memory
        .remember_with_parts(cid, "assistant", "tool output", parts_json, None)
        .await
        .unwrap();
    let msg_id = msg_id_opt.unwrap();
    assert!(msg_id > MessageId(0));

    let history = memory.sqlite.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content, "tool output");
}

#[tokio::test]
async fn effective_embed_provider_routes_to_dedicated_embed_provider() {
    // main_provider has embeddings disabled
    let main_provider = AnyProvider::Mock(MockProvider::default());
    // embed_provider has embeddings enabled
    let embed_provider = AnyProvider::Mock(MockProvider::default().with_embedding(vec![0.1]));

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let memory = SemanticMemory {
        sqlite,
        qdrant: None,
        provider: main_provider,
        embed_provider: Some(embed_provider),
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay_enabled: false,
        temporal_decay_half_life_days: 30,
        mmr_enabled: false,
        mmr_lambda: 0.7,
        importance_enabled: false,
        importance_weight: 0.15,
        token_counter: Arc::new(TokenCounter::new()),
        graph_store: None,
        experience: None,
        reasoning: None,
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
        depth_below_limit_warned: Arc::new(AtomicBool::new(false)),
        missing_placeholder_warned: Arc::new(AtomicBool::new(false)),
    };

    assert!(
        memory.effective_embed_provider().supports_embeddings(),
        "effective_embed_provider() must return the dedicated embed provider"
    );
    assert!(
        !memory.provider.supports_embeddings(),
        "main provider must not support embeddings, proving the two providers are distinct"
    );
}

#[tokio::test]
async fn has_embedding_without_qdrant() {
    let memory = test_semantic_memory(true).await;

    let has_embedding = memory.has_embedding(MessageId(1)).await.unwrap();
    assert!(!has_embedding);
}

#[tokio::test]
async fn embed_missing_without_qdrant() {
    let memory = test_semantic_memory(true).await;

    let count = memory.embed_missing(None).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn sqlite_accessor() {
    let memory = test_semantic_memory(false).await;

    let cid = memory.sqlite().create_conversation().await.unwrap();
    assert_eq!(cid, ConversationId(1));

    memory
        .sqlite()
        .save_message(cid, "user", "test")
        .await
        .unwrap();

    let history = memory.sqlite().load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 1);
}

#[tokio::test]
async fn has_vector_store_returns_false_when_unavailable() {
    let memory = test_semantic_memory(false).await;
    assert!(!memory.has_vector_store());
}

#[tokio::test]
async fn is_vector_store_connected_returns_false_when_unavailable() {
    let memory = test_semantic_memory(false).await;
    assert!(!memory.is_vector_store_connected().await);
}

#[tokio::test]
async fn embed_missing_returns_zero_when_embeddings_not_supported() {
    let memory = test_semantic_memory(false).await;

    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .sqlite()
        .save_message(cid, "user", "test")
        .await
        .unwrap();

    let count = memory.embed_missing(None).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn message_count_empty_conversation() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let count = memory.message_count(cid).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn message_count_after_saves() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "msg1", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid, "assistant", "msg2", None)
        .await
        .unwrap()
        .unwrap();

    let count = memory.message_count(cid).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn remember_multiple_messages_increments_ids() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    let id1 = memory
        .remember(cid, "user", "first", None)
        .await
        .unwrap()
        .unwrap();
    let id2 = memory
        .remember(cid, "assistant", "second", None)
        .await
        .unwrap()
        .unwrap();
    let id3 = memory
        .remember(cid, "user", "third", None)
        .await
        .unwrap()
        .unwrap();

    assert!(id1 < id2);
    assert!(id2 < id3);
}

#[tokio::test]
async fn message_count_across_conversations() {
    let memory = test_semantic_memory(false).await;
    let cid1 = memory.sqlite().create_conversation().await.unwrap();
    let cid2 = memory.sqlite().create_conversation().await.unwrap();

    memory
        .remember(cid1, "user", "msg1", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid1, "user", "msg2", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid2, "user", "msg3", None)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(memory.message_count(cid1).await.unwrap(), 2);
    assert_eq!(memory.message_count(cid2).await.unwrap(), 1);
}

#[tokio::test]
async fn message_count_multiple_conversations_isolated() {
    let memory = test_semantic_memory(false).await;
    let cid1 = memory.sqlite().create_conversation().await.unwrap();
    let cid2 = memory.sqlite().create_conversation().await.unwrap();
    let cid3 = memory.sqlite().create_conversation().await.unwrap();

    for _ in 0..5 {
        memory
            .remember(cid1, "user", "msg", None)
            .await
            .unwrap()
            .unwrap();
    }
    for _ in 0..3 {
        memory
            .remember(cid2, "user", "msg", None)
            .await
            .unwrap()
            .unwrap();
    }

    assert_eq!(memory.message_count(cid1).await.unwrap(), 5);
    assert_eq!(memory.message_count(cid2).await.unwrap(), 3);
    assert_eq!(memory.message_count(cid3).await.unwrap(), 0);
}

#[tokio::test]
async fn remember_with_embeddings_supported_but_no_qdrant() {
    let memory = test_semantic_memory(true).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    let msg_id = memory
        .remember(cid, "user", "hello embed", None)
        .await
        .unwrap()
        .unwrap();
    assert!(msg_id > MessageId(0));

    let history = memory.sqlite.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content, "hello embed");
}

#[tokio::test]
async fn remember_verifies_content_via_load_history() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "alpha", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid, "assistant", "beta", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid, "user", "gamma", None)
        .await
        .unwrap()
        .unwrap();

    let history = memory.sqlite().load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].content, "alpha");
    assert_eq!(history[1].content, "beta");
    assert_eq!(history[2].content, "gamma");
}

#[tokio::test]
async fn remember_preserves_role_mapping() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    memory
        .remember(cid, "user", "u", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid, "assistant", "a", None)
        .await
        .unwrap()
        .unwrap();
    memory
        .remember(cid, "system", "s", None)
        .await
        .unwrap()
        .unwrap();

    let history = memory.sqlite.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[2].role, Role::System);
}

#[tokio::test]
async fn new_with_invalid_qdrant_url_graceful() {
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    let provider = AnyProvider::Mock(mock);
    let result =
        SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn has_embedding_returns_false_when_no_qdrant() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();
    let msg_id = memory
        .remember(cid, "user", "test", None)
        .await
        .unwrap()
        .unwrap();
    assert!(!memory.has_embedding(msg_id).await.unwrap());
}

#[tokio::test]
async fn message_count_nonexistent_conversation() {
    let memory = test_semantic_memory(false).await;
    let count = memory.message_count(ConversationId(999)).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn store_session_summary_no_qdrant_noop() {
    let memory = test_semantic_memory(true).await;
    let result = memory
        .store_session_summary(ConversationId(1), "test summary")
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn store_session_summary_no_embeddings_noop() {
    let memory = test_semantic_memory(false).await;
    let result = memory
        .store_session_summary(ConversationId(1), "test summary")
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn search_session_summaries_no_qdrant_empty() {
    let memory = test_semantic_memory(true).await;
    let results = memory
        .search_session_summaries("query", 5, None)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn search_session_summaries_no_embeddings_empty() {
    let memory = test_semantic_memory(false).await;
    let results = memory
        .search_session_summaries("query", 5, Some(ConversationId(1)))
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn store_correction_embedding_no_qdrant_noop() {
    let memory = test_semantic_memory(true).await;
    let result = memory.store_correction_embedding(1, "bad response").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn store_correction_embedding_no_embeddings_noop() {
    let memory = test_semantic_memory(false).await;
    let result = memory.store_correction_embedding(1, "bad response").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn retrieve_similar_corrections_no_qdrant_empty() {
    let memory = test_semantic_memory(true).await;
    let results = memory
        .retrieve_similar_corrections("query", 5, 0.0)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn retrieve_similar_corrections_no_embeddings_empty() {
    let memory = test_semantic_memory(false).await;
    let results = memory
        .retrieve_similar_corrections("query", 5, 0.0)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn store_correction_embedding_sqlite_clean_db_roundtrip() {
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
        temporal_decay_enabled: false,
        temporal_decay_half_life_days: 30,
        mmr_enabled: false,
        mmr_lambda: 0.7,
        importance_enabled: false,
        importance_weight: 0.15,
        token_counter: Arc::new(TokenCounter::new()),
        graph_store: None,
        experience: None,
        reasoning: None,
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
        depth_below_limit_warned: Arc::new(AtomicBool::new(false)),
        missing_placeholder_warned: Arc::new(AtomicBool::new(false)),
    };

    memory
        .store_correction_embedding(1, "bad response")
        .await
        .unwrap();

    let results = memory
        .retrieve_similar_corrections("bad", 5, 0.0)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[test]
fn session_summary_result_debug() {
    let result = SessionSummaryResult {
        summary_text: "test".into(),
        score: 0.9,
        conversation_id: ConversationId(1),
    };
    let dbg = format!("{result:?}");
    assert!(dbg.contains("SessionSummaryResult"));
}

#[test]
fn session_summary_result_clone() {
    let result = SessionSummaryResult {
        summary_text: "test".into(),
        score: 0.9,
        conversation_id: ConversationId(1),
    };
    let cloned = result.clone();
    assert_eq!(result.summary_text, cloned.summary_text);
    assert_eq!(result.conversation_id, cloned.conversation_id);
}

use proptest::prelude::*;

proptest! {
    #[test]
    fn count_tokens_never_panics(s in ".*") {
        use std::sync::LazyLock;
        static COUNTER: LazyLock<TokenCounter> = LazyLock::new(TokenCounter::new);
        let _ = COUNTER.count_tokens(&s);
    }
}
