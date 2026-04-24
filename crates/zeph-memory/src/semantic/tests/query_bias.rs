// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit tests for MM-F3: `classify_query_intent` and `apply_query_bias`.

use super::*;
use crate::semantic::{CachedCentroid, QueryIntent, SemanticMemory};
use std::time::Instant;

// ── classify_query_intent ─────────────────────────────────────────────────────

#[test]
fn test_classify_query_intent_first_person() {
    assert_eq!(
        SemanticMemory::classify_query_intent("What did I say about Rust?"),
        QueryIntent::FirstPerson
    );
    assert_eq!(
        SemanticMemory::classify_query_intent("Tell me about my projects"),
        QueryIntent::FirstPerson
    );
    assert_eq!(
        SemanticMemory::classify_query_intent("What are my preferences?"),
        QueryIntent::FirstPerson
    );
}

#[test]
fn test_classify_query_intent_other() {
    // Queries without first-person pronouns must classify as Other.
    // Note: "Tell me about ..." contains " me " and would match FirstPerson — use neutral queries.
    assert_eq!(
        SemanticMemory::classify_query_intent("What is the capital of France?"),
        QueryIntent::Other
    );
    assert_eq!(
        SemanticMemory::classify_query_intent("How does Rust ownership work?"),
        QueryIntent::Other
    );
    assert_eq!(
        SemanticMemory::classify_query_intent("The team uses Python for scripting"),
        QueryIntent::Other
    );
}

// ── apply_query_bias ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_apply_query_bias_disabled_returns_unchanged() {
    let memory = test_semantic_memory(false).await;
    // query_bias_correction is false by default in test_semantic_memory.
    let embedding = vec![0.1_f32, 0.2, 0.3];
    let result = memory
        .apply_query_bias("What did I say about Rust?", embedding.clone())
        .await;
    assert_eq!(
        result, embedding,
        "disabled bias correction must return the original embedding unchanged"
    );
}

#[tokio::test]
async fn test_apply_query_bias_dimension_mismatch_returns_unchanged() {
    // Construct a SemanticMemory with bias correction enabled.
    let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
    let memory = SemanticMemory {
        sqlite,
        qdrant: None,
        provider: test_provider(),
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
        token_counter: Arc::new(crate::token_counter::TokenCounter::new()),
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
        query_bias_correction: true,
        query_bias_profile_weight: 0.25,
        // Inject a centroid with a different dimension (2) than the query embedding (3).
        profile_centroid: tokio::sync::RwLock::new(Some(CachedCentroid {
            vector: vec![1.0_f32, 2.0_f32],
            computed_at: Instant::now(),
        })),
        profile_centroid_ttl_secs: 300,
        hebbian_enabled: false,
        hebbian_lr: 0.1,
    };

    let embedding = vec![0.1_f32, 0.2, 0.3];
    // "What did I say" triggers FirstPerson intent.
    let result = memory
        .apply_query_bias("What did I say about Rust?", embedding.clone())
        .await;
    assert_eq!(
        result, embedding,
        "dimension mismatch must return the original embedding unchanged"
    );
}
