# Semantic Memory

Enable semantic search to retrieve contextually relevant messages from conversation history using vector similarity.

Requires an embedding model. Ollama with `qwen3-embedding` is the default. Claude API does not support embeddings natively — use the [orchestrator](../advanced/orchestrator.md) to route embeddings through Ollama while using Claude for chat.

## Vector Backend

Zeph supports two vector backends for storing embeddings:

| Backend | Best for | External dependencies |
|---------|----------|----------------------|
| `qdrant` (default) | Production, multi-user, large datasets | Qdrant server |
| `sqlite` | Development, single-user, offline, quick setup | None |

The `sqlite` backend stores vectors in the same SQLite database as conversation history and performs cosine similarity search in-process. It requires no external services, making it ideal for local development and single-user deployments.

## Setup with SQLite Backend (Quickstart)

No external services needed:

```toml
[memory]
vector_backend = "sqlite"

[memory.semantic]
enabled = true
recall_limit = 5
```

The vector tables are created automatically via migration `011_vector_store.sql`.

## Setup with Qdrant Backend

1. **Start Qdrant:**

   ```bash
   docker compose up -d qdrant
   ```

2. **Enable semantic memory in config:**

   ```toml
   [memory]
   vector_backend = "qdrant"  # default, can be omitted

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

## Temporal Decay

Enable time-based score attenuation to prefer recent context over stale information:

```toml
[memory.semantic]
temporal_decay_enabled = true
temporal_decay_half_life_days = 30  # Score halves every 30 days
```

Scores decay exponentially: at 1 half-life a message retains 50% of its original score, at 2 half-lives 25%, and so on. Adjust `temporal_decay_half_life_days` based on how quickly your project context changes.

## MMR Re-ranking

Enable Maximal Marginal Relevance to diversify recall results and reduce redundancy:

```toml
[memory.semantic]
mmr_enabled = true
mmr_lambda = 0.7  # 0.0 = max diversity, 1.0 = pure relevance
```

MMR iteratively selects results that are both relevant to the query and dissimilar to already-selected items. The default `mmr_lambda = 0.7` works well for most use cases. Lower it if you see too many semantically similar results in recall.

## Autosave Assistant Responses

By default, only user messages are embedded. Enable `autosave_assistant` to also embed assistant responses for richer semantic recall:

```toml
[memory]
autosave_assistant = true
autosave_min_length = 20  # Skip embedding for very short replies
```

Short responses (below `autosave_min_length` bytes) are still saved to SQLite but skip the embedding step. User messages always generate embeddings regardless of this setting.

## Memory Export and Import

Back up or migrate conversation data with portable JSON snapshots:

```bash
zeph memory export conversations.json
zeph memory import conversations.json
```

See [CLI Reference — `zeph memory`](../reference/cli.md#zeph-memory) for details.

## Storage Architecture

| Store | Purpose |
|-------|---------|
| SQLite | Source of truth for message text, conversations, summaries, skill usage |
| Qdrant or SQLite vectors | Vector index for semantic similarity search (embeddings only) |

Both stores work together: SQLite holds the data, the vector backend enables similarity search over it. With the Qdrant backend, the `embeddings_metadata` table in SQLite maps message IDs to Qdrant point IDs. With the SQLite backend, vectors are stored directly in `vector_points` and `vector_point_payloads` tables.
