// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use super::*;
use crate::in_memory_store::InMemoryVectorStore;
use crate::store::SqliteStore;

async fn setup() -> GraphStore {
    let store = SqliteStore::new(":memory:").await.unwrap();
    GraphStore::new(store.pool().clone())
}

async fn setup_with_embedding() -> (GraphStore, Arc<EmbeddingStore>) {
    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let mem_store = Box::new(InMemoryVectorStore::new());
    let emb = Arc::new(EmbeddingStore::with_store(mem_store, pool));
    let gs = GraphStore::new(sqlite.pool().clone());
    (gs, emb)
}

fn make_mock_provider_with_embedding(embedding: Vec<f32>) -> zeph_llm::mock::MockProvider {
    let mut p = zeph_llm::mock::MockProvider::default();
    p.embedding = embedding;
    p.supports_embeddings = true;
    p
}

// ── Existing tests (resolve() with no embedding store — exact match only) ──

#[tokio::test]
async fn resolve_creates_new_entity() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);
    let (id, outcome) = resolver
        .resolve("alice", "person", Some("a person"))
        .await
        .unwrap();
    assert!(id > 0);
    assert_eq!(outcome, ResolutionOutcome::Created);
}

#[tokio::test]
async fn resolve_updates_existing_entity() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);
    let (id1, _) = resolver.resolve("alice", "person", None).await.unwrap();
    let (id2, outcome) = resolver
        .resolve("alice", "person", Some("updated summary"))
        .await
        .unwrap();
    assert_eq!(id1, id2);
    assert_eq!(outcome, ResolutionOutcome::ExactMatch);

    let entity = gs
        .find_entity("alice", EntityType::Person)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.summary.as_deref(), Some("updated summary"));
}

#[tokio::test]
async fn resolve_unknown_type_falls_back_to_concept() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);
    let (id, _) = resolver
        .resolve("my_thing", "unknown_type", None)
        .await
        .unwrap();
    assert!(id > 0);

    // Verify it was stored as Concept
    let entity = gs
        .find_entity("my_thing", EntityType::Concept)
        .await
        .unwrap();
    assert!(entity.is_some());
}

#[tokio::test]
async fn resolve_empty_name_returns_error() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let result_empty = resolver.resolve("", "concept", None).await;
    assert!(result_empty.is_err());
    assert!(matches!(
        result_empty.unwrap_err(),
        MemoryError::GraphStore(_)
    ));

    let result_whitespace = resolver.resolve("   ", "concept", None).await;
    assert!(result_whitespace.is_err());
}

// FIX-3 defense-in-depth: short entity names must be rejected at the resolver level.
#[tokio::test]
async fn resolve_short_name_below_min_returns_error() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // "go" and "cd" are 2-byte tokens that represent common noise from tool output.
    let err_go = resolver.resolve("go", "technology", None).await;
    assert!(err_go.is_err(), "\"go\" (2 bytes) must be rejected");
    assert!(
        matches!(err_go.unwrap_err(), MemoryError::GraphStore(_)),
        "expected GraphStore error for short name"
    );

    let err_cd = resolver.resolve("cd", "concept", None).await;
    assert!(err_cd.is_err(), "\"cd\" (2 bytes) must be rejected");
}

#[tokio::test]
async fn resolve_name_at_min_length_passes() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // "git" is exactly 3 bytes — must be accepted.
    let result = resolver.resolve("git", "technology", None).await;
    assert!(
        result.is_ok(),
        "\"git\" (3 bytes) must pass min-length check"
    );
}

#[tokio::test]
async fn resolve_case_insensitive() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let (id1, _) = resolver.resolve("Rust", "language", None).await.unwrap();
    let (id2, outcome) = resolver.resolve("rust", "language", None).await.unwrap();
    assert_eq!(
        id1, id2,
        "'Rust' and 'rust' should resolve to the same entity"
    );
    assert_eq!(outcome, ResolutionOutcome::ExactMatch);
}

#[tokio::test]
async fn resolve_edge_inserts_new() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let src = gs
        .upsert_entity("src", "src", EntityType::Concept, None)
        .await
        .unwrap()
        .0;
    let tgt = gs
        .upsert_entity("tgt", "tgt", EntityType::Concept, None)
        .await
        .unwrap()
        .0;

    let result = resolver
        .resolve_edge(src, tgt, "uses", "src uses tgt", 0.9, None)
        .await
        .unwrap();
    assert!(result.is_some());
    assert!(result.unwrap() > 0);
}

#[tokio::test]
async fn resolve_edge_deduplicates_identical() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let src = gs
        .upsert_entity("a", "a", EntityType::Concept, None)
        .await
        .unwrap()
        .0;
    let tgt = gs
        .upsert_entity("b", "b", EntityType::Concept, None)
        .await
        .unwrap()
        .0;

    let first = resolver
        .resolve_edge(src, tgt, "uses", "a uses b", 0.9, None)
        .await
        .unwrap();
    assert!(first.is_some());

    let second = resolver
        .resolve_edge(src, tgt, "uses", "a uses b", 0.9, None)
        .await
        .unwrap();
    assert!(second.is_none(), "identical edge should be deduplicated");
}

#[tokio::test]
async fn resolve_edge_supersedes_contradictory() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let src = gs
        .upsert_entity("x", "x", EntityType::Concept, None)
        .await
        .unwrap()
        .0;
    let tgt = gs
        .upsert_entity("y", "y", EntityType::Concept, None)
        .await
        .unwrap()
        .0;

    let first_id = resolver
        .resolve_edge(src, tgt, "prefers", "x prefers y (old)", 0.8, None)
        .await
        .unwrap()
        .unwrap();

    let second_id = resolver
        .resolve_edge(src, tgt, "prefers", "x prefers y (new)", 0.9, None)
        .await
        .unwrap()
        .unwrap();

    assert_ne!(first_id, second_id, "superseded edge should have a new ID");

    // Old edge should be invalidated
    let active_count = gs.active_edge_count().await.unwrap();
    assert_eq!(active_count, 1, "only new edge should be active");
}

#[tokio::test]
async fn resolve_edge_direction_sensitive() {
    // A->B "uses" should not interfere with B->A "uses" dedup
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let a = gs
        .upsert_entity("node_a", "node_a", EntityType::Concept, None)
        .await
        .unwrap()
        .0;
    let b = gs
        .upsert_entity("node_b", "node_b", EntityType::Concept, None)
        .await
        .unwrap()
        .0;

    // Insert A->B
    let id1 = resolver
        .resolve_edge(a, b, "uses", "A uses B", 0.9, None)
        .await
        .unwrap();
    assert!(id1.is_some());

    // Insert B->A with different fact — should NOT invalidate A->B (different direction)
    let id2 = resolver
        .resolve_edge(b, a, "uses", "B uses A (different direction)", 0.9, None)
        .await
        .unwrap();
    assert!(id2.is_some());

    // Both edges should still be active
    let active_count = gs.active_edge_count().await.unwrap();
    assert_eq!(active_count, 2, "both directional edges should be active");
}

#[tokio::test]
async fn resolve_edge_normalizes_relation_case() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let src = gs
        .upsert_entity("p", "p", EntityType::Concept, None)
        .await
        .unwrap()
        .0;
    let tgt = gs
        .upsert_entity("q", "q", EntityType::Concept, None)
        .await
        .unwrap()
        .0;

    // Insert with uppercase relation
    let id1 = resolver
        .resolve_edge(src, tgt, "Uses", "p uses q", 0.9, None)
        .await
        .unwrap();
    assert!(id1.is_some());

    // Insert with lowercase relation — same normalized relation, same fact → deduplicate
    let id2 = resolver
        .resolve_edge(src, tgt, "uses", "p uses q", 0.9, None)
        .await
        .unwrap();
    assert!(id2.is_none(), "normalized relations should deduplicate");
}

// ── IC-01: entity_type lowercased before parse ────────────────────────────

#[tokio::test]
async fn resolve_entity_type_uppercase_parsed_correctly() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // "Person" (title case from LLM) should parse as EntityType::Person, not fall back to Concept
    let (id, _) = resolver
        .resolve("test_entity", "Person", None)
        .await
        .unwrap();
    assert!(id > 0);

    let entity = gs
        .find_entity("test_entity", EntityType::Person)
        .await
        .unwrap();
    assert!(entity.is_some(), "entity should be stored as Person type");
}

#[tokio::test]
async fn resolve_entity_type_all_caps_parsed_correctly() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let (id, _) = resolver.resolve("my_lang", "LANGUAGE", None).await.unwrap();
    assert!(id > 0);

    let entity = gs
        .find_entity("my_lang", EntityType::Language)
        .await
        .unwrap();
    assert!(entity.is_some(), "entity should be stored as Language type");
}

// ── SEC-GRAPH-01: entity name length cap ──────────────────────────────────

#[tokio::test]
async fn resolve_truncates_long_entity_name() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let long_name = "a".repeat(1024);
    let (id, _) = resolver.resolve(&long_name, "concept", None).await.unwrap();
    assert!(id > 0);

    // Entity should exist with a truncated name (512 bytes)
    let entity = gs
        .find_entity(&"a".repeat(512), EntityType::Concept)
        .await
        .unwrap();
    assert!(entity.is_some(), "truncated name should be stored");
}

// ── SEC-GRAPH-02: control character stripping ─────────────────────────────

#[tokio::test]
async fn resolve_strips_control_chars_from_name() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // Name with null byte and a BiDi override
    let name_with_ctrl = "rust\x00lang";
    let (id, _) = resolver
        .resolve(name_with_ctrl, "language", None)
        .await
        .unwrap();
    assert!(id > 0);

    // Stored name should have control chars removed
    let entity = gs
        .find_entity("rustlang", EntityType::Language)
        .await
        .unwrap();
    assert!(
        entity.is_some(),
        "control chars should be stripped from stored name"
    );
}

#[tokio::test]
async fn resolve_strips_bidi_overrides_from_name() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // U+202E is RIGHT-TO-LEFT OVERRIDE — a BiDi spoof character
    let name_with_bidi = "rust\u{202E}lang";
    let (id, _) = resolver
        .resolve(name_with_bidi, "language", None)
        .await
        .unwrap();
    assert!(id > 0);

    let entity = gs
        .find_entity("rustlang", EntityType::Language)
        .await
        .unwrap();
    assert!(entity.is_some(), "BiDi override chars should be stripped");
}

// ── Helper unit tests for sanitization functions ──────────────────────────

#[test]
fn strip_control_chars_removes_ascii_controls() {
    assert_eq!(strip_control_chars("hello\x00world"), "helloworld");
    assert_eq!(strip_control_chars("tab\there"), "tabhere");
    assert_eq!(strip_control_chars("new\nline"), "newline");
}

#[test]
fn strip_control_chars_removes_bidi() {
    let bidi = "\u{202E}spoof";
    assert_eq!(strip_control_chars(bidi), "spoof");
}

#[test]
fn strip_control_chars_preserves_normal_unicode() {
    assert_eq!(strip_control_chars("привет мир"), "привет мир");
    assert_eq!(strip_control_chars("日本語"), "日本語");
}

#[test]
fn truncate_to_bytes_exact_boundary() {
    let s = "hello";
    assert_eq!(truncate_to_bytes_ref(s, 5), "hello");
    assert_eq!(truncate_to_bytes_ref(s, 3), "hel");
}

#[test]
fn truncate_to_bytes_respects_utf8_boundary() {
    // "é" is 2 bytes in UTF-8 — truncating at 1 byte should give ""
    let s = "élan";
    let truncated = truncate_to_bytes_ref(s, 1);
    assert!(s.is_char_boundary(truncated.len()));
}

// ── New tests: embedding-based resolution ─────────────────────────────────

#[tokio::test]
async fn resolve_with_embedding_store_score_above_threshold_merges() {
    let (gs, emb) = setup_with_embedding().await;
    // Pre-insert an existing entity (different name to avoid exact match).
    // "python programming lang" is in Qdrant; we resolve "python scripting lang"
    // which embeds to the identical vector → cosine similarity = 1.0 > 0.85 → merge.
    let existing_id = gs
        .upsert_entity(
            "python programming lang",
            "python programming lang",
            EntityType::Language,
            Some("a programming language"),
        )
        .await
        .unwrap()
        .0;

    let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
    emb.ensure_named_collection(ENTITY_COLLECTION, 4)
        .await
        .unwrap();
    let payload = serde_json::json!({
        "entity_id": existing_id,
        "name": "python programming lang",
        "entity_type": "language",
        "summary": "a programming language",
    });
    let point_id = emb
        .store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
        .await
        .unwrap();
    gs.set_entity_qdrant_point_id(existing_id, &point_id)
        .await
        .unwrap();

    // Mock provider returns the same vector for any text → cosine similarity = 1.0 > 0.85
    let provider = make_mock_provider_with_embedding(mock_vec);
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.70);

    // Resolve a different name — no exact match, embedding match wins
    let (id, outcome) = resolver
        .resolve(
            "python scripting lang",
            "language",
            Some("scripting language"),
        )
        .await
        .unwrap();

    assert_eq!(id, existing_id, "should return existing entity ID on merge");
    assert!(
        matches!(outcome, ResolutionOutcome::EmbeddingMatch { score } if score > 0.85),
        "outcome should be EmbeddingMatch with score > 0.85, got {outcome:?}"
    );
}

#[tokio::test]
async fn resolve_with_embedding_store_score_below_ambiguous_creates_new() {
    let (gs, emb) = setup_with_embedding().await;
    // Insert existing entity with orthogonal vector
    let existing_id = gs
        .upsert_entity("java", "java", EntityType::Language, Some("java language"))
        .await
        .unwrap()
        .0;

    // Existing uses [1,0,0,0]; new entity will embed to [0,1,0,0] (orthogonal, score=0)
    emb.ensure_named_collection(ENTITY_COLLECTION, 4)
        .await
        .unwrap();
    let payload = serde_json::json!({
        "entity_id": existing_id,
        "name": "java",
        "entity_type": "language",
        "summary": "java language",
    });
    emb.store_to_collection(ENTITY_COLLECTION, payload, vec![1.0, 0.0, 0.0, 0.0])
        .await
        .unwrap();

    // Mock returns orthogonal vector → score = 0.0 < 0.70
    let provider = make_mock_provider_with_embedding(vec![0.0, 1.0, 0.0, 0.0]);
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.70);

    let (id, outcome) = resolver
        .resolve("kotlin", "language", Some("kotlin language"))
        .await
        .unwrap();

    assert_ne!(id, existing_id, "orthogonal entity should create new");
    assert_eq!(outcome, ResolutionOutcome::Created);
}

#[tokio::test]
async fn resolve_with_embedding_failure_falls_back_to_create() {
    // Use a mock with supports_embeddings=false — embed() returns EmbedUnsupported error,
    // which triggers the fallback path (create new entity).
    let sqlite2 = SqliteStore::new(":memory:").await.unwrap();
    let pool2 = sqlite2.pool().clone();
    let mem2 = Box::new(InMemoryVectorStore::new());
    let emb2 = Arc::new(EmbeddingStore::with_store(mem2, pool2));
    let gs2 = GraphStore::new(sqlite2.pool().clone());

    let mut mock = zeph_llm::mock::MockProvider::default();
    mock.supports_embeddings = false;
    let any_provider = zeph_llm::any::AnyProvider::Mock(mock);

    let resolver = EntityResolver::new(&gs2)
        .with_embedding_store(&emb2)
        .with_provider(&any_provider);

    let (id, outcome) = resolver
        .resolve("testentity", "concept", Some("summary"))
        .await
        .unwrap();
    assert!(id > 0);
    assert_eq!(outcome, ResolutionOutcome::Created);
}

#[tokio::test]
async fn resolve_fallback_increments_counter() {
    let (gs, emb) = setup_with_embedding().await;

    // Provider with embed that fails (supports_embeddings=false → EmbedUnsupported error)
    let mut mock = zeph_llm::mock::MockProvider::default();
    mock.supports_embeddings = false;
    let any_provider = zeph_llm::any::AnyProvider::Mock(mock);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider);

    let fallback_count = resolver.fallback_count();

    // First call: embed fails → fallback
    resolver.resolve("entity_a", "concept", None).await.unwrap();

    assert_eq!(
        fallback_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "fallback counter should be 1 after embed failure"
    );
}

#[tokio::test]
async fn resolve_batch_processes_multiple_entities() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let entities = vec![
        ExtractedEntity {
            name: "rust".into(),
            entity_type: "language".into(),
            summary: Some("systems language".into()),
        },
        ExtractedEntity {
            name: "python".into(),
            entity_type: "language".into(),
            summary: None,
        },
        ExtractedEntity {
            name: "cargo".into(),
            entity_type: "tool".into(),
            summary: Some("rust build tool".into()),
        },
    ];

    let results = resolver.resolve_batch(&entities).await.unwrap();
    assert_eq!(results.len(), 3);
    for (id, outcome) in &results {
        assert!(*id > 0);
        assert_eq!(*outcome, ResolutionOutcome::Created);
    }
}

#[tokio::test]
async fn resolve_batch_empty_returns_empty() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);
    let results = resolver.resolve_batch(&[]).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn merge_combines_summaries() {
    let (gs, emb) = setup_with_embedding().await;
    // Use a different name for the existing entity so exact match doesn't trigger.
    // "mergetest v1" is stored with embedding; we then resolve "mergetest v2" which
    // embeds to the same vector → similarity = 1.0 > threshold → merge.
    let existing_id = gs
        .upsert_entity(
            "mergetest v1",
            "mergetest v1",
            EntityType::Concept,
            Some("first summary"),
        )
        .await
        .unwrap()
        .0;

    let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
    emb.ensure_named_collection(ENTITY_COLLECTION, 4)
        .await
        .unwrap();
    let payload = serde_json::json!({
        "entity_id": existing_id,
        "name": "mergetest v1",
        "entity_type": "concept",
        "summary": "first summary",
    });
    let point_id = emb
        .store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
        .await
        .unwrap();
    gs.set_entity_qdrant_point_id(existing_id, &point_id)
        .await
        .unwrap();

    let provider = make_mock_provider_with_embedding(mock_vec);
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.70);

    // Resolve "mergetest v2" — no exact match, but embedding is identical → merge
    let (id, outcome) = resolver
        .resolve("mergetest v2", "concept", Some("second summary"))
        .await
        .unwrap();

    assert_eq!(id, existing_id);
    assert!(matches!(outcome, ResolutionOutcome::EmbeddingMatch { .. }));

    // Verify the merged summary was updated on the existing entity
    let entity = gs
        .find_entity("mergetest v1", EntityType::Concept)
        .await
        .unwrap()
        .unwrap();
    let summary = entity.summary.unwrap_or_default();
    assert!(
        summary.contains("first summary") && summary.contains("second summary"),
        "merged summary should contain both: got {summary:?}"
    );
}

#[tokio::test]
async fn merge_preserves_older_entity_id() {
    let (gs, emb) = setup_with_embedding().await;
    // "legacy entity" stored with embedding; "legacy entity variant" has same vector → merge
    let existing_id = gs
        .upsert_entity(
            "legacy entity",
            "legacy entity",
            EntityType::Concept,
            Some("old info"),
        )
        .await
        .unwrap()
        .0;

    let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
    emb.ensure_named_collection(ENTITY_COLLECTION, 4)
        .await
        .unwrap();
    let payload = serde_json::json!({
        "entity_id": existing_id,
        "name": "legacy entity",
        "entity_type": "concept",
        "summary": "old info",
    });
    emb.store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
        .await
        .unwrap();

    let provider = make_mock_provider_with_embedding(mock_vec);
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.70);

    let (returned_id, _) = resolver
        .resolve("legacy entity variant", "concept", Some("new info"))
        .await
        .unwrap();

    assert_eq!(
        returned_id, existing_id,
        "older entity ID should be preserved on merge"
    );
}

#[tokio::test]
async fn entity_type_filter_prevents_cross_type_merge() {
    let (gs, emb) = setup_with_embedding().await;

    // Insert a Person named "python"
    let person_id = gs
        .upsert_entity(
            "python",
            "python",
            EntityType::Person,
            Some("a person named python"),
        )
        .await
        .unwrap()
        .0;

    let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
    emb.ensure_named_collection(ENTITY_COLLECTION, 4)
        .await
        .unwrap();
    let payload = serde_json::json!({
        "entity_id": person_id,
        "name": "python",
        "entity_type": "person",
        "summary": "a person named python",
    });
    emb.store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
        .await
        .unwrap();

    let provider = make_mock_provider_with_embedding(mock_vec);
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.70);

    // Resolve "python" as Language — should NOT merge with the Person entity
    let (lang_id, outcome) = resolver
        .resolve("python", "language", Some("python language"))
        .await
        .unwrap();

    // The entity_type filter should prevent merging person "python" with language "python"
    // Check: either created new or an exact match was found under language type
    assert_ne!(
        lang_id, person_id,
        "language entity should not merge with person entity"
    );
    // The entity_type filter causes no embedding candidate to survive the type filter,
    // so resolution falls back to creating a new entity.
    assert_eq!(outcome, ResolutionOutcome::Created);
}

#[tokio::test]
async fn custom_thresholds_respected() {
    let (gs, emb) = setup_with_embedding().await;
    // With a very high threshold (1.0), even identical vectors won't merge
    // (they'd score exactly 1.0 which is NOT > 1.0, so... let's use 0.5 threshold
    // and verify score below 0.5 creates new)
    let existing_id = gs
        .upsert_entity(
            "threshold_test",
            "threshold_test",
            EntityType::Concept,
            Some("base"),
        )
        .await
        .unwrap()
        .0;

    let existing_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
    emb.ensure_named_collection(ENTITY_COLLECTION, 4)
        .await
        .unwrap();
    let payload = serde_json::json!({
        "entity_id": existing_id,
        "name": "threshold_test",
        "entity_type": "concept",
        "summary": "base",
    });
    emb.store_to_collection(ENTITY_COLLECTION, payload, existing_vec)
        .await
        .unwrap();

    // Orthogonal vector → score = 0.0
    let provider = make_mock_provider_with_embedding(vec![0.0, 1.0, 0.0, 0.0]);
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    // With thresholds 0.50/0.30, score=0 is below 0.30 → create new
    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.50, 0.30);

    let (id, outcome) = resolver
        .resolve("new_concept", "concept", Some("different"))
        .await
        .unwrap();

    assert_ne!(id, existing_id);
    assert_eq!(outcome, ResolutionOutcome::Created);
}

#[tokio::test]
async fn resolve_outcome_exact_match_no_embedding_store() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    resolver.resolve("existing", "concept", None).await.unwrap();
    let (_, outcome) = resolver.resolve("existing", "concept", None).await.unwrap();
    assert_eq!(outcome, ResolutionOutcome::ExactMatch);
}

#[tokio::test]
async fn extract_json_strips_markdown_fences() {
    let with_fence = "```json\n{\"same_entity\": true}\n```";
    let extracted = extract_json(with_fence);
    let parsed: DisambiguationResponse = serde_json::from_str(extracted).unwrap();
    assert!(parsed.same_entity);

    let without_fence = "{\"same_entity\": false}";
    let extracted2 = extract_json(without_fence);
    let parsed2: DisambiguationResponse = serde_json::from_str(extracted2).unwrap();
    assert!(!parsed2.same_entity);
}

// Helper: build a MockProvider with embeddings enabled, given vector, and queued chat responses.
fn make_mock_with_embedding_and_chat(
    embedding: Vec<f32>,
    chat_responses: Vec<String>,
) -> zeph_llm::mock::MockProvider {
    let mut p = zeph_llm::mock::MockProvider::with_responses(chat_responses);
    p.embedding = embedding;
    p.supports_embeddings = true;
    p
}

// Seed an existing entity into both SQLite and InMemoryVectorStore at a known vector.
async fn seed_entity_with_vector(
    gs: &GraphStore,
    emb: &Arc<EmbeddingStore>,
    name: &str,
    entity_type: EntityType,
    summary: &str,
    vector: Vec<f32>,
) -> i64 {
    let id = gs
        .upsert_entity(name, name, entity_type, Some(summary))
        .await
        .unwrap()
        .0;
    emb.ensure_named_collection(ENTITY_COLLECTION, u64::try_from(vector.len()).unwrap())
        .await
        .unwrap();
    let payload = serde_json::json!({
        "entity_id": id,
        "name": name,
        "entity_type": entity_type.as_str(),
        "summary": summary,
    });
    let point_id = emb
        .store_to_collection(ENTITY_COLLECTION, payload, vector)
        .await
        .unwrap();
    gs.set_entity_qdrant_point_id(id, &point_id).await.unwrap();
    id
}

// ── GAP-1: ambiguous score + LLM says same_entity=true → LlmDisambiguated ─

#[tokio::test]
async fn resolve_ambiguous_score_llm_says_merge() {
    // existing entity at [1,0,0,0]; new entity embeds to [1,1,0,0] → cosine ≈ 0.707
    // thresholds: similarity=0.85, ambiguous=0.50 → score 0.707 is in [0.50, 0.85)
    let (gs, emb) = setup_with_embedding().await;
    let existing_id = seed_entity_with_vector(
        &gs,
        &emb,
        "goroutine",
        EntityType::Concept,
        "go concurrency primitive",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;

    // LLM responds with same_entity=true → should merge
    let provider = make_mock_with_embedding_and_chat(
        vec![1.0, 1.0, 0.0, 0.0],
        vec![r#"{"same_entity": true}"#.to_owned()],
    );
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.50);

    let (id, outcome) = resolver
        .resolve("goroutine concurrency", "concept", Some("go concurrency"))
        .await
        .unwrap();

    assert_eq!(
        id, existing_id,
        "should return existing entity ID on LLM merge"
    );
    assert_eq!(outcome, ResolutionOutcome::LlmDisambiguated);
}

// ── GAP-2: ambiguous score + LLM says same_entity=false → Created ──────────

#[tokio::test]
async fn resolve_ambiguous_score_llm_says_different() {
    let (gs, emb) = setup_with_embedding().await;
    let existing_id = seed_entity_with_vector(
        &gs,
        &emb,
        "channel",
        EntityType::Concept,
        "go channel",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;

    // LLM responds with same_entity=false → should create new entity
    let provider = make_mock_with_embedding_and_chat(
        vec![1.0, 1.0, 0.0, 0.0],
        vec![r#"{"same_entity": false}"#.to_owned()],
    );
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.50);

    let (id, outcome) = resolver
        .resolve("network channel", "concept", Some("networking channel"))
        .await
        .unwrap();

    assert_ne!(
        id, existing_id,
        "LLM-rejected match should create new entity"
    );
    assert_eq!(outcome, ResolutionOutcome::Created);
}

// ── GAP-3: ambiguous score + LLM chat fails → fallback counter incremented ─

#[tokio::test]
async fn resolve_ambiguous_score_llm_failure_increments_fallback() {
    let (gs, emb) = setup_with_embedding().await;
    let existing_id = seed_entity_with_vector(
        &gs,
        &emb,
        "mutex",
        EntityType::Concept,
        "mutual exclusion lock",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;

    // fail_chat=true → provider.chat() returns Err → None from llm_disambiguate
    let mut provider = make_mock_with_embedding_and_chat(vec![1.0, 1.0, 0.0, 0.0], vec![]);
    provider.fail_chat = true;
    let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

    let resolver = EntityResolver::new(&gs)
        .with_embedding_store(&emb)
        .with_provider(&any_provider)
        .with_thresholds(0.85, 0.50);

    let fallback_count = resolver.fallback_count();

    let (id, outcome) = resolver
        .resolve("mutex lock", "concept", Some("synchronization primitive"))
        .await
        .unwrap();

    // LLM failure → fallback to create new
    assert_ne!(
        id, existing_id,
        "LLM failure should create new entity (fallback)"
    );
    assert_eq!(outcome, ResolutionOutcome::Created);
    assert_eq!(
        fallback_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "fallback counter should be incremented on LLM chat failure"
    );
}

// ── Canonicalization / alias tests ────────────────────────────────────────

#[tokio::test]
async fn resolve_creates_entity_with_canonical_name() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);
    let (id, _) = resolver.resolve("Rust", "language", None).await.unwrap();
    assert!(id > 0);
    let entity = gs
        .find_entity("rust", EntityType::Language)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.canonical_name, "rust");
}

#[tokio::test]
async fn resolve_adds_alias_on_create() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);
    let (id, _) = resolver.resolve("Rust", "language", None).await.unwrap();
    let aliases = gs.aliases_for_entity(id).await.unwrap();
    assert!(
        !aliases.is_empty(),
        "new entity should have at least one alias"
    );
    assert!(aliases.iter().any(|a| a.alias_name == "rust"));
}

#[tokio::test]
async fn resolve_reuses_entity_by_alias() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // Create entity and register an alias
    let (id1, _) = resolver.resolve("rust", "language", None).await.unwrap();
    gs.add_alias(id1, "rust-lang").await.unwrap();

    // Resolve using the alias — should return the same entity
    let (id2, _) = resolver
        .resolve("rust-lang", "language", None)
        .await
        .unwrap();
    assert_eq!(
        id1, id2,
        "'rust-lang' alias should resolve to same entity as 'rust'"
    );
}

#[tokio::test]
async fn resolve_alias_match_respects_entity_type() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // "python" as a Language
    let (lang_id, _) = resolver.resolve("python", "language", None).await.unwrap();

    // "python" as a Tool should create a separate entity (different type)
    let (tool_id, _) = resolver.resolve("python", "tool", None).await.unwrap();
    assert_ne!(
        lang_id, tool_id,
        "same name with different type should be separate entities"
    );
}

#[tokio::test]
async fn resolve_preserves_existing_aliases() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    let (id, _) = resolver.resolve("rust", "language", None).await.unwrap();
    gs.add_alias(id, "rust-lang").await.unwrap();

    // Upserting same entity should not remove prior aliases
    resolver
        .resolve("rust", "language", Some("updated"))
        .await
        .unwrap();
    let aliases = gs.aliases_for_entity(id).await.unwrap();
    assert!(
        aliases.iter().any(|a| a.alias_name == "rust-lang"),
        "prior alias must be preserved"
    );
}

#[tokio::test]
async fn resolve_original_form_registered_as_alias() {
    let gs = setup().await;
    let resolver = EntityResolver::new(&gs);

    // "  Rust  " — original trimmed lowercased form is "rust", same as normalized
    // So only one alias should be registered (no duplicate)
    let (id, _) = resolver
        .resolve("  Rust  ", "language", None)
        .await
        .unwrap();
    let aliases = gs.aliases_for_entity(id).await.unwrap();
    assert!(aliases.iter().any(|a| a.alias_name == "rust"));
}

#[tokio::test]
async fn resolve_entity_with_many_aliases() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("bigentity", "bigentity", EntityType::Concept, None)
        .await
        .unwrap()
        .0;
    for i in 0..100 {
        gs.add_alias(id, &format!("alias-{i}")).await.unwrap();
    }
    let aliases = gs.aliases_for_entity(id).await.unwrap();
    assert_eq!(aliases.len(), 100);

    // Fuzzy search should still work via alias
    let results = gs.find_entities_fuzzy("alias-50", 10).await.unwrap();
    assert!(results.iter().any(|e| e.id.0 == id));
}
