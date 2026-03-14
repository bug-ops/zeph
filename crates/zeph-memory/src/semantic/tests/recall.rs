// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::Role;

use crate::embedding_store::SearchFilter;
use crate::sqlite::SqliteStore;
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
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay_enabled: false,
        temporal_decay_half_life_days: 30,
        mmr_enabled: false,
        mmr_lambda: 0.7,
        token_counter: Arc::new(TokenCounter::new()),
        graph_store: None,
        community_detection_failures: Arc::new(AtomicU64::new(0)),
        graph_extraction_count: Arc::new(AtomicU64::new(0)),
        graph_extraction_failures: Arc::new(AtomicU64::new(0)),
    };

    let cid = memory.sqlite().create_conversation().await.unwrap();

    let id1 = memory
        .remember(cid, "user", "rust async programming")
        .await
        .unwrap();
    let id2 = memory
        .remember(cid, "assistant", "use tokio for async")
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

    let count = memory.embed_missing().await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn recall_empty_without_qdrant_regardless_of_filter() {
    let memory = test_semantic_memory(true).await;
    let filter = SearchFilter {
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
        .remember(cid, "user", "rust programming guide")
        .await
        .unwrap();
    memory
        .remember(cid, "assistant", "python tutorial")
        .await
        .unwrap();
    memory
        .remember(cid, "user", "advanced rust patterns")
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

    memory.remember(cid1, "user", "hello world").await.unwrap();
    memory
        .remember(cid2, "user", "hello universe")
        .await
        .unwrap();

    let filter = SearchFilter {
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

    memory.remember(cid, "user", "hello world").await.unwrap();

    let recalled = memory.recall("nonexistent", 5, None).await.unwrap();
    assert!(recalled.is_empty());
}

#[tokio::test]
async fn recall_fts5_respects_limit() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite.create_conversation().await.unwrap();

    for i in 0..10 {
        memory
            .remember(cid, "user", &format!("test message number {i}"))
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
        .remember(cid, "user", "rust programming guide")
        .await
        .unwrap();
    memory
        .remember(cid, "assistant", "python tutorial")
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(router.route("rust_guide"), MemoryRoute::Keyword);

    let recalled = memory
        .recall_routed("rust_guide", 5, None, &router)
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
        .remember(cid, "user", "how does the agent loop work")
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(
        router.route("how does the agent loop work"),
        MemoryRoute::Semantic
    );

    let recalled = memory
        .recall_routed("how does the agent loop work", 5, None, &router)
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
        .remember(cid, "user", "context window token budget")
        .await
        .unwrap();

    let router = HeuristicRouter;
    assert_eq!(
        router.route("context window token budget"),
        MemoryRoute::Hybrid
    );

    let recalled = memory
        .recall_routed("context window token budget", 5, None, &router)
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
        .remember(cid, "user", "we should discuss rust ownership")
        .await
        .unwrap();
    memory
        .remember(cid, "assistant", "python tutorial instead")
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
                conversation_id: Some(cid),
                role: None,
            }),
            &router,
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
        .remember(cid, "user", "last time we deployed the service it broke")
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
                conversation_id: Some(cid),
                role: None,
            }),
            &router,
        )
        .await
        .unwrap();
    assert!(
        !recalled.is_empty(),
        "fallback to original query must find the message containing 'last time'"
    );
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
