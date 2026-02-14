# Semantic Memory

Enable semantic search to retrieve contextually relevant messages from conversation history using vector similarity.

Requires an embedding model. Ollama with `qwen3-embedding` is the default. Claude API does not support embeddings natively — use the [orchestrator](orchestrator.md) to route embeddings through Ollama while using Claude for chat.

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

- **Automatic embedding:** Messages are embedded asynchronously using the configured `embedding_model` and stored in Qdrant alongside SQLite.
- **Semantic recall:** Context builder injects semantically relevant messages from full history, not just recent messages.
- **Graceful degradation:** If Qdrant is unavailable, Zeph falls back to SQLite-only mode (recency-based history).
- **Startup backfill:** On startup, if Qdrant is available, Zeph calls `embed_missing()` to backfill embeddings for any messages stored while Qdrant was offline. This ensures the vector index stays in sync with SQLite without manual intervention.

## Storage Architecture

| Store | Purpose |
|-------|---------|
| SQLite | Source of truth for message text, conversations, summaries, skill usage |
| Qdrant | Vector index for semantic similarity search (embeddings only) |

Both stores work together: SQLite holds the data, Qdrant enables vector search over it. The `embeddings_metadata` table in SQLite maps message IDs to Qdrant point IDs.
