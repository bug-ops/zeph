// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::ollama::OllamaProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::response_cache::ResponseCache;
use zeph_memory::store::SqliteStore;

const OLLAMA_BASE_URL: &str = "http://localhost:11434";

fn ollama_chat_model() -> String {
    std::env::var("OLLAMA_CHAT_MODEL").unwrap_or_else(|_| "qwen3:8b".into())
}

fn ollama_embed_model() -> String {
    std::env::var("OLLAMA_EMBED_MODEL").unwrap_or_else(|_| "qwen3-embedding".into())
}

// Cold Ollama model starts can take 30+ seconds on first embed call.
async fn setup_cache_with_ollama() -> (ResponseCache, OllamaProvider) {
    let store = SqliteStore::new(":memory:")
        .await
        .expect("in-memory SQLite must open");
    let pool = store.pool().clone();
    let cache = ResponseCache::new(pool, 3600);
    let provider = OllamaProvider::new(OLLAMA_BASE_URL, ollama_chat_model(), ollama_embed_model());
    (cache, provider)
}

#[tokio::test]
#[ignore = "requires running Ollama instance with qwen3-embedding model"]
async fn with_ollama_embedding() {
    // Roundtrip: embed a query, store via put_with_embedding, retrieve with the same
    // embedding. Identical vectors must yield score ~1.0.
    let (cache, provider) = setup_cache_with_ollama().await;

    let query = "What is the Rust programming language?";
    let embedding = provider
        .embed(query)
        .await
        .expect("Ollama embed must succeed");

    assert!(!embedding.is_empty(), "embedding must not be empty");
    assert!(
        embedding.len() > 100,
        "embedding must have more than 100 dimensions"
    );
    assert!(
        embedding.iter().all(|v| v.is_finite()),
        "all embedding values must be finite"
    );

    cache
        .put_with_embedding(
            "k1",
            "Rust is a systems programming language",
            &ollama_chat_model(),
            &embedding,
            &ollama_embed_model(),
        )
        .await
        .expect("put_with_embedding must succeed");

    let result = cache
        .get_semantic(&embedding, &ollama_embed_model(), 0.95, 10)
        .await
        .expect("get_semantic must succeed");

    let (response, score) = result.expect("identical embedding must produce a cache hit");
    assert_eq!(response, "Rust is a systems programming language");
    assert!(
        (score - 1.0_f32).abs() < 1e-5,
        "identical embedding must yield score ~1.0, got {score}"
    );
}

#[tokio::test]
#[ignore = "requires running Ollama instance with qwen3-embedding model"]
async fn hit_on_rephrase() {
    // Threshold 0.80 provides margin for embedding model version variance.
    // Semantically equivalent rephrases typically score 0.85–0.98 with qwen3-embedding.
    let (cache, provider) = setup_cache_with_ollama().await;

    let original = "What is the capital of France?";
    let embedding_original = provider
        .embed(original)
        .await
        .expect("Ollama embed must succeed for original query");

    cache
        .put_with_embedding(
            "k1",
            "Paris is the capital of France",
            &ollama_chat_model(),
            &embedding_original,
            &ollama_embed_model(),
        )
        .await
        .expect("put_with_embedding must succeed");

    let rephrase = "Tell me the capital city of France";
    let embedding_rephrase = provider
        .embed(rephrase)
        .await
        .expect("Ollama embed must succeed for rephrased query");

    let result = cache
        .get_semantic(&embedding_rephrase, &ollama_embed_model(), 0.80, 10)
        .await
        .expect("get_semantic must succeed");

    let (_response, score) = result.expect("rephrase must hit semantic cache at threshold 0.80");
    assert!(
        score > 0.80,
        "rephrase similarity must exceed 0.80, got {score}"
    );
}

#[tokio::test]
#[ignore = "requires running Ollama instance with qwen3-embedding model"]
async fn threshold_boundary() {
    // Verify that threshold correctly separates hits from misses.
    // An unrelated query (Rust ownership vs pasta recipes) must miss at 0.95
    // but hit at 0.0 since cosine similarity >= 0.0 for any stored entry.
    let (cache, provider) = setup_cache_with_ollama().await;

    let tech_query = "Explain Rust ownership and borrowing";
    let embedding_tech = provider
        .embed(tech_query)
        .await
        .expect("Ollama embed must succeed for tech query");

    cache
        .put_with_embedding(
            "k1",
            "Rust ownership ensures memory safety without GC",
            &ollama_chat_model(),
            &embedding_tech,
            &ollama_embed_model(),
        )
        .await
        .expect("put_with_embedding must succeed");

    let unrelated = "Best Italian pasta recipes for beginners";
    let embedding_unrelated = provider
        .embed(unrelated)
        .await
        .expect("Ollama embed must succeed for unrelated query");

    let miss = cache
        .get_semantic(&embedding_unrelated, &ollama_embed_model(), 0.95, 10)
        .await
        .expect("get_semantic must succeed");
    assert!(
        miss.is_none(),
        "unrelated query must not hit cache at threshold 0.95"
    );

    // Threshold 0.0 guarantees a hit: cosine similarity for any real embedding pair
    // stored in the cache is >= 0.0 for typical non-adversarial inputs.
    let hit = cache
        .get_semantic(&embedding_unrelated, &ollama_embed_model(), 0.0, 10)
        .await
        .expect("get_semantic must succeed");
    assert!(
        hit.is_some(),
        "any non-negative similarity must pass threshold 0.0"
    );
}
