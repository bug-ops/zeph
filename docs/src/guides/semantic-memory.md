# Semantic Memory

Enable semantic search to retrieve contextually relevant messages from conversation history using vector similarity.

Requires an embedding model. Ollama with `qwen3-embedding` is the default. Claude API does not support embeddings natively — use the [orchestrator](../advanced/orchestrator.md) to route embeddings through Ollama while using Claude for chat.

## Setup

1. **Start Qdrant:**

   ```bash
   docker compose up -d qdrant
   ```

2. **Enable semantic memory in config:**

   ```toml
   [memory.semantic]
   enabled = true
   recall_limit = 5
   ```

3. **Automatic setup:** Qdrant collection (`zeph_conversations`) is created automatically on first use with correct vector dimensions (1024 for `qwen3-embedding`) and Cosine distance metric. No manual initialization required.

## How It Works

- **Hybrid search:** Recall uses both Qdrant vector similarity and SQLite FTS5 keyword search, merging results with configurable weights. This improves recall quality especially for exact term matches.
- **Automatic embedding:** Messages are embedded asynchronously using the configured `embedding_model` and stored in Qdrant alongside SQLite.
- **FTS5 index:** All messages are automatically indexed in an SQLite FTS5 virtual table via triggers, enabling BM25-ranked keyword search with zero configuration.
- **Graceful degradation:** If Qdrant is unavailable, Zeph falls back to FTS5-only keyword search instead of returning empty results.
- **Startup backfill:** On startup, if Qdrant is available, Zeph calls `embed_missing()` to backfill embeddings for any messages stored while Qdrant was offline.

## Hybrid Search Weights

Configure the balance between vector (semantic) and keyword (BM25) search:

```toml
[memory.semantic]
enabled = true
recall_limit = 5
vector_weight = 0.7   # Weight for Qdrant vector similarity
keyword_weight = 0.3  # Weight for FTS5 keyword relevance
```

When Qdrant is unavailable, only keyword search runs (effectively `keyword_weight = 1.0`).

## Storage Architecture

| Store | Purpose |
|-------|---------|
| SQLite | Source of truth for message text, conversations, summaries, skill usage |
| Qdrant | Vector index for semantic similarity search (embeddings only) |

Both stores work together: SQLite holds the data, Qdrant enables vector search over it. The `embeddings_metadata` table in SQLite maps message IDs to Qdrant point IDs.
