// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use testcontainers::ContainerAsync;
use testcontainers::GenericImage;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_memory::QdrantOps;
use zeph_memory::embedding_registry::{EmbedFuture, Embeddable, EmbeddingRegistry};
use zeph_memory::embedding_store::{EmbeddingStore, MessageKind};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::store::SqliteStore;

const QDRANT_GRPC_PORT: ContainerPort = ContainerPort::Tcp(6334);

fn qdrant_image() -> GenericImage {
    GenericImage::new("qdrant/qdrant", "v1.16.0")
        .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
        .with_wait_for(WaitFor::seconds(1))
        .with_exposed_port(QDRANT_GRPC_PORT)
}

async fn setup_with_qdrant() -> (SqliteStore, EmbeddingStore, ContainerAsync<GenericImage>) {
    let container = qdrant_image().start().await.unwrap();
    let grpc_port = container.get_host_port_ipv4(6334).await.unwrap();
    let url = format!("http://127.0.0.1:{grpc_port}");

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let store = EmbeddingStore::new(&url, None, pool).unwrap();

    (sqlite, store, container)
}

#[tokio::test]
#[ignore = "requires Docker for Qdrant"]
async fn ensure_collection_is_idempotent() {
    let (_sqlite, qdrant, _container) = setup_with_qdrant().await;

    qdrant.ensure_collection(768).await.unwrap();
    qdrant.ensure_collection(768).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker for Qdrant"]
async fn store_and_search_vector() {
    let (sqlite, qdrant, _container) = setup_with_qdrant().await;

    let cid = sqlite.create_conversation().await.unwrap();
    let msg_id = sqlite
        .save_message(cid, "user", "hello world")
        .await
        .unwrap();

    qdrant.ensure_collection(4).await.unwrap();

    let vector = vec![0.1, 0.2, 0.3, 0.4];
    let point_id = qdrant
        .store(
            msg_id,
            cid,
            "user",
            vector.clone(),
            MessageKind::Regular,
            "qwen3-embedding",
            0,
            None,
        )
        .await
        .unwrap();

    assert!(!point_id.is_empty());
    assert!(qdrant.has_embedding(msg_id).await.unwrap());

    let results = qdrant.search(&vector, 10, None).await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].message_id, msg_id);
}

#[tokio::test]
#[ignore = "requires Docker for Qdrant"]
async fn search_with_conversation_filter() {
    let (sqlite, qdrant, _container) = setup_with_qdrant().await;

    let cid1 = sqlite.create_conversation().await.unwrap();
    let cid2 = sqlite.create_conversation().await.unwrap();

    let msg1 = sqlite.save_message(cid1, "user", "first").await.unwrap();
    let msg2 = sqlite.save_message(cid2, "user", "second").await.unwrap();

    qdrant.ensure_collection(4).await.unwrap();

    let v1 = vec![0.1, 0.2, 0.3, 0.4];
    let v2 = vec![0.1, 0.2, 0.3, 0.5];

    qdrant
        .store(
            msg1,
            cid1,
            "user",
            v1,
            MessageKind::Regular,
            "qwen3-embedding",
            0,
            None,
        )
        .await
        .unwrap();
    qdrant
        .store(
            msg2,
            cid2,
            "user",
            v2,
            MessageKind::Regular,
            "qwen3-embedding",
            0,
            None,
        )
        .await
        .unwrap();

    let query = vec![0.1, 0.2, 0.3, 0.4];
    let filter = zeph_memory::embedding_store::SearchFilter {
        conversation_id: Some(cid1),
        role: None,
        category: None,
    };

    let results = qdrant.search(&query, 10, Some(filter)).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].conversation_id, cid1);
}

// --- SemanticMemory integration tests ---

async fn setup_semantic_memory_with_qdrant() -> (SemanticMemory, ContainerAsync<GenericImage>) {
    let container = qdrant_image().start().await.unwrap();
    let grpc_port = container.get_host_port_ipv4(6334).await.unwrap();
    let url = format!("http://127.0.0.1:{grpc_port}");

    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    mock.embedding = vec![0.1_f32; 384];
    let provider = AnyProvider::Mock(mock);

    let memory = SemanticMemory::new(":memory:", &url, None, provider, "test-model")
        .await
        .unwrap();

    (memory, container)
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn store_session_summary_roundtrip() {
    let (memory, _container) = setup_semantic_memory_with_qdrant().await;

    // Guard: Qdrant client must be configured and reachable.
    assert!(
        memory.has_vector_store(),
        "Qdrant client must be configured"
    );
    assert!(
        memory.is_vector_store_connected().await,
        "Qdrant must be reachable"
    );

    let cid = memory.sqlite().create_conversation().await.unwrap();
    let summary = "Discussed Rust async patterns and error handling";

    memory.store_session_summary(cid, summary).await.unwrap();

    let results = memory
        .search_session_summaries("Rust async", 5, None)
        .await
        .unwrap();

    assert_eq!(results.len(), 1, "must find the stored summary");
    assert_eq!(results[0].summary_text, summary);
    assert_eq!(results[0].conversation_id, cid);
    // MockProvider returns identical vectors; cosine similarity is always 1.0.
    // Qdrant may return 1.0000001 due to f32 rounding — allow a tiny epsilon.
    assert!(
        (results[0].score - 1.0_f32).abs() < 1e-5,
        "expected score ≈ 1.0 (identical vectors), got {}",
        results[0].score
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn store_session_summary_multiple_conversations() {
    let (memory, _container) = setup_semantic_memory_with_qdrant().await;

    assert!(
        memory.has_vector_store(),
        "Qdrant client must be configured"
    );
    assert!(
        memory.is_vector_store_connected().await,
        "Qdrant must be reachable"
    );

    let cid_a = memory.sqlite().create_conversation().await.unwrap();
    let cid_b = memory.sqlite().create_conversation().await.unwrap();
    let cid_c = memory.sqlite().create_conversation().await.unwrap();

    memory
        .store_session_summary(cid_a, "summary A about databases")
        .await
        .unwrap();
    memory
        .store_session_summary(cid_b, "summary B about testing")
        .await
        .unwrap();
    memory
        .store_session_summary(cid_c, "summary C about networking")
        .await
        .unwrap();

    // All three summaries must be reachable before applying any filter.
    let all = memory
        .search_session_summaries("query", 10, None)
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        3,
        "unfiltered search must return all 3 summaries"
    );

    // Excluding cid_a must return exactly the remaining two conversations.
    let filtered = memory
        .search_session_summaries("query", 10, Some(cid_a))
        .await
        .unwrap();
    assert_eq!(
        filtered.len(),
        2,
        "filtered search must exclude cid_a, returning 2 results"
    );
    assert!(
        filtered.iter().all(|r| r.conversation_id != cid_a),
        "cid_a must not appear in results after exclusion"
    );
    assert!(
        filtered.iter().any(|r| r.conversation_id == cid_b),
        "cid_b must be present in filtered results"
    );
    assert!(
        filtered.iter().any(|r| r.conversation_id == cid_c),
        "cid_c must be present in filtered results"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn store_shutdown_summary_full_roundtrip() {
    let (memory, _container) = setup_semantic_memory_with_qdrant().await;

    assert!(
        memory.has_vector_store(),
        "Qdrant client must be configured"
    );
    assert!(
        memory.is_vector_store_connected().await,
        "Qdrant must be reachable"
    );

    let cid = memory.sqlite().create_conversation().await.unwrap();
    let summary = "shutdown summary: discussed deployment strategies";

    // Pass empty key_facts to keep the test focused on the summary path only.
    memory
        .store_shutdown_summary(cid, summary, &[])
        .await
        .unwrap();

    // SQLite must record the summary (authoritative path) with the correct text.
    assert!(
        memory.has_session_summary(cid).await.unwrap(),
        "SQLite must record the shutdown summary"
    );
    let stored = memory.sqlite().load_summaries(cid).await.unwrap();
    assert_eq!(stored.len(), 1, "SQLite must have exactly one summary");
    assert_eq!(
        stored[0].2, summary,
        "SQLite summary text must match what was stored"
    );

    // Qdrant must also have received the upsert (vector search path).
    let results = memory
        .search_session_summaries("deployment", 5, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1, "Qdrant must return the stored summary");
    assert_eq!(results[0].summary_text, summary);
    assert_eq!(results[0].conversation_id, cid);
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn search_session_summaries_returns_empty_when_no_data() {
    let (memory, _container) = setup_semantic_memory_with_qdrant().await;

    assert!(
        memory.has_vector_store(),
        "Qdrant client must be configured"
    );
    assert!(
        memory.is_vector_store_connected().await,
        "Qdrant must be reachable"
    );

    // search_session_summaries creates the collection on first call; with no stored
    // points the result must be an empty Vec, not an error.
    let results = memory
        .search_session_summaries("anything", 5, None)
        .await
        .unwrap();
    assert!(
        results.is_empty(),
        "search on empty collection must return empty results"
    );
}

// --- EmbeddingRegistry::get_vectors_by_keys integration tests (issue #5786) ---
//
// These prove the `id_to_key` remapping logic against a real Qdrant instance: unit tests
// against an unreachable Qdrant (in embedding_registry.rs) only exercise the error path,
// never the happy path where points are actually found and remapped back to caller keys.

struct TestSkill {
    key: String,
    text: String,
}

impl Embeddable for TestSkill {
    fn key(&self) -> &str {
        &self.key
    }

    fn content_hash(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.text.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    fn embed_text(&self) -> &str {
        &self.text
    }

    fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({"key": self.key, "text": self.text})
    }
}

fn make_skill(key: &str, text: &str) -> TestSkill {
    TestSkill {
        key: key.into(),
        text: text.into(),
    }
}

async fn setup_registry_with_qdrant(
    collection: &str,
) -> (EmbeddingRegistry, ContainerAsync<GenericImage>) {
    let container = qdrant_image().start().await.unwrap();
    let grpc_port = container.get_host_port_ipv4(6334).await.unwrap();
    let url = format!("http://127.0.0.1:{grpc_port}");

    let ops = QdrantOps::new(&url, None).unwrap();
    let ns = uuid::Uuid::from_bytes([0u8; 16]);
    let registry = EmbeddingRegistry::new(ops, collection, ns);

    (registry, container)
}

#[allow(clippy::cast_precision_loss)]
fn embed_fn_fixed(dim: usize) -> impl Fn(&str) -> EmbedFuture {
    move |text: &str| -> EmbedFuture {
        // Deterministic per-text vector so distinct skills get distinct (but stable) vectors.
        let seed = text.bytes().map(u32::from).sum::<u32>() as f32;
        let vec = (0..dim)
            .map(|i| (seed + i as f32) / 1000.0)
            .collect::<Vec<f32>>();
        Box::pin(async move { Ok(vec) })
    }
}

#[tokio::test]
#[ignore = "requires Docker for Qdrant"]
async fn get_vectors_by_keys_happy_path_returns_stored_vectors() {
    let (mut registry, _container) = setup_registry_with_qdrant("test_get_vectors_happy").await;

    let skills = vec![
        make_skill("skill-a", "alpha description"),
        make_skill("skill-b", "beta description"),
        make_skill("skill-c", "gamma description"),
    ];
    let embed_fn = embed_fn_fixed(4);
    let stats = registry
        .sync(&skills, "test-model", &embed_fn, None)
        .await
        .unwrap();
    assert_eq!(stats.added, 3);

    let keys = vec!["skill-a".to_string(), "skill-b".to_string()];
    let result = registry.get_vectors_by_keys(&keys).await.unwrap();

    assert_eq!(result.len(), 2, "must return exactly the requested keys");
    assert!(result.contains_key("skill-a"));
    assert!(result.contains_key("skill-b"));
    assert!(!result.contains_key("skill-c"), "must not over-fetch");

    // Qdrant collections created with `Distance::Cosine` normalize stored vectors to unit
    // length, so the retrieved vector isn't byte-identical to what was embedded — only
    // direction is preserved. Assert dimension and direction (cosine similarity ~= 1.0)
    // rather than raw equality.
    let expected_a = embed_fn("alpha description").await.unwrap();
    let expected_b = embed_fn("beta description").await.unwrap();
    assert_eq!(result["skill-a"].len(), expected_a.len());
    assert_eq!(result["skill-b"].len(), expected_b.len());
    assert!(
        (zeph_common::math::cosine_similarity(&result["skill-a"], &expected_a) - 1.0).abs() < 1e-4,
        "retrieved vector must point in the same direction as the embedded one"
    );
    assert!(
        (zeph_common::math::cosine_similarity(&result["skill-b"], &expected_b) - 1.0).abs() < 1e-4,
        "retrieved vector must point in the same direction as the embedded one"
    );
}

#[tokio::test]
#[ignore = "requires Docker for Qdrant"]
async fn get_vectors_by_keys_partial_miss_omits_unstored_keys() {
    let (mut registry, _container) = setup_registry_with_qdrant("test_get_vectors_partial").await;

    let skills = vec![make_skill("skill-real", "real skill description")];
    let embed_fn = embed_fn_fixed(4);
    registry
        .sync(&skills, "test-model", &embed_fn, None)
        .await
        .unwrap();

    // Request one key that was synced and one that was never stored.
    let keys = vec!["skill-real".to_string(), "skill-never-stored".to_string()];
    let result = registry.get_vectors_by_keys(&keys).await.unwrap();

    assert_eq!(
        result.len(),
        1,
        "must silently omit keys with no matching point, not error"
    );
    assert!(result.contains_key("skill-real"));
    assert!(!result.contains_key("skill-never-stored"));
}
