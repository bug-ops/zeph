// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;

use crate::sqlite::SqliteStore;
use crate::token_counter::TokenCounter;
use crate::types::{ConversationId, MessageId};

use super::super::*;
use super::test_semantic_memory;

#[tokio::test]
async fn unsummarized_count_decreases_after_summary() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..10 {
        memory
            .remember(cid, "user", &format!("msg{i}"))
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

    let msg_id1 = memory.remember(cid, "user", "m1").await.unwrap().unwrap();
    let msg_id2 = memory
        .remember(cid, "assistant", "m2")
        .await
        .unwrap()
        .unwrap();
    let msg_id3 = memory.remember(cid, "user", "m3").await.unwrap().unwrap();

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
        .remember(cid, "user", "hello")
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
            .remember(cid, "user", &format!("message {i}"))
            .await
            .unwrap();
    }

    let summary_id = memory.summarize(cid, 3).await.unwrap();
    assert!(summary_id.is_some());

    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, summary_id.unwrap());
    assert!(!summaries[0].content.is_empty());
}

#[tokio::test]
async fn summarize_respects_previous_summaries() {
    let memory = test_semantic_memory(false).await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..10 {
        memory
            .remember(cid, "user", &format!("message {i}"))
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
            .remember(cid, "user", &format!("msg {i}"))
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
            .remember(cid, "user", &format!("msg {i}"))
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
            .remember(cid, "user", &format!("msg {i}"))
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
            .remember(cid, "user", &format!("msg {i}"))
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
            .remember(cid, "user", &format!("message {i}"))
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
        embedding_model: "test".into(),
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
        community_detection_failures: Arc::new(AtomicU64::new(0)),
        graph_extraction_count: Arc::new(AtomicU64::new(0)),
        graph_extraction_failures: Arc::new(AtomicU64::new(0)),
        tier_boost_semantic: 1.3,
        admission_control: None,
    };
    let cid = memory.sqlite().create_conversation().await.unwrap();

    for i in 0..5 {
        memory
            .remember(cid, "user", &format!("msg {i}"))
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
            .remember(cid, "user", &format!("msg {i}"))
            .await
            .unwrap();
    }

    let summary_id = memory.summarize(cid, 4).await.unwrap().unwrap();
    let summaries = memory.load_summaries(cid).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, summary_id);
    assert!(summaries[0].first_message_id >= Some(MessageId(1)));
    assert!(summaries[0].last_message_id >= summaries[0].first_message_id);
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
        embedding_model: "test".into(),
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
        community_detection_failures: Arc::new(AtomicU64::new(0)),
        graph_extraction_count: Arc::new(AtomicU64::new(0)),
        graph_extraction_failures: Arc::new(AtomicU64::new(0)),
        tier_boost_semantic: 1.3,
        admission_control: None,
    };

    let cid = memory.sqlite().create_conversation().await.unwrap();
    for i in 0..5 {
        memory
            .remember(cid, "user", &format!("msg {i}"))
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
    let facts = memory.search_key_facts("query", 5).await.unwrap();
    assert!(facts.is_empty());
}

#[tokio::test]
async fn load_summaries_nonexistent_conversation() {
    let memory = test_semantic_memory(false).await;
    let summaries = memory.load_summaries(ConversationId(999)).await.unwrap();
    assert!(summaries.is_empty());
}
