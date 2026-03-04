# Memory and Context

Zeph uses a dual-store memory system: SQLite for structured conversation history and a configurable vector backend (Qdrant or embedded SQLite) for semantic search across past sessions.

## Conversation History

All messages are stored in SQLite. The CLI channel provides persistent input history with arrow-key navigation, prefix search, and Emacs keybindings. History persists across restarts.

When conversations grow long, Zeph compacts history automatically (triggered when token usage exceeds `compaction_threshold`). Compaction uses dual-visibility flags on each message: original messages are marked `agent_visible=false` (hidden from the LLM) while remaining `user_visible=true` (preserved in UI). A summary is inserted as `agent_visible=true, user_visible=false` — visible to the LLM but hidden from the user. This is performed atomically via `replace_conversation()` in SQLite. The result: the user retains full scroll-back history while the LLM operates on a compact context.

## Semantic Memory

With semantic memory enabled, messages are embedded as vectors for similarity search. Ask "what did we discuss about the API yesterday?" and Zeph retrieves relevant context from past sessions automatically. Both vector similarity and keyword (FTS5) search respect visibility boundaries — only `agent_visible=true` messages are indexed and returned, so compacted originals never appear in recall results.

Two vector backends are available:

| Backend | Use case | Dependency |
|---------|----------|------------|
| `qdrant` (default) | Production, large datasets | External Qdrant server |
| `sqlite` | Development, single-user, offline | None (embedded) |

Semantic memory uses hybrid search — vector similarity combined with SQLite FTS5 keyword search — to improve recall quality. When the vector backend is unavailable, Zeph falls back to keyword-only search.

### Result Quality: MMR and Temporal Decay

Two post-processing stages improve recall quality beyond raw similarity:

- **Temporal decay** attenuates scores based on message age. A configurable half-life (default: 30 days) ensures recent context is preferred over stale information. Scores decay exponentially: a message at 1 half-life gets 50% weight, at 2 half-lives 25%, etc.
- **MMR re-ranking** (Maximal Marginal Relevance) reduces redundancy in results by penalizing candidates too similar to already-selected items. The `mmr_lambda` parameter (default: 0.7) controls the relevance-diversity trade-off: higher values favor relevance, lower values favor diversity.

Both are disabled by default. Enable them in `[memory.semantic]`:

```toml
[memory.semantic]
enabled = true
recall_limit = 5
temporal_decay_enabled = true
temporal_decay_half_life_days = 30
mmr_enabled = true
mmr_lambda = 0.7
```

### Quick Setup

Embedded SQLite vectors (no external dependencies):

```toml
[memory]
vector_backend = "sqlite"

[memory.semantic]
enabled = true
recall_limit = 5
```

Qdrant (production):

```toml
[memory]
vector_backend = "qdrant"  # default

[memory.semantic]
enabled = true
recall_limit = 5
```

See [Set Up Semantic Memory](../guides/semantic-memory.md) for the full setup guide.

## Context Engineering

Token counts throughout the context pipeline are computed by `TokenCounter` — a shared BPE tokenizer (`cl100k_base`) with a DashMap cache. This replaced the previous `chars / 4` heuristic and provides accurate budget allocation, especially for non-ASCII content and tool schemas. See [Token Efficiency — Token Counting](../architecture/token-efficiency.md#token-counting) for implementation details.

When `context_budget_tokens` is set (default: 0 = unlimited), Zeph allocates the context window proportionally:

| Allocation | Share | Purpose |
|-----------|-------|---------|
| Summaries | 15% | Compressed conversation history |
| Semantic recall | 25% | Relevant messages from past sessions |
| Recent history | 60% | Most recent messages in current conversation |

A two-tier pruning system manages overflow:

1. **Tool output pruning** (cheap) — replaces old tool outputs with short placeholders
2. **Chunked LLM compaction** (fallback) — splits middle messages into ~4096-token chunks, summarizes them in parallel (up to 4 concurrent LLM calls), then merges partial summaries. Falls back to single-pass if any chunk fails.

Both tiers run automatically. See [Context Engineering](../advanced/context.md) for tuning options.

## Project Context

Drop a `ZEPH.md` file in your project root and Zeph discovers it automatically. Project-specific instructions are included in every prompt as a `<project_context>` block. Zeph walks up the directory tree looking for `ZEPH.md`, `ZEPH.local.md`, or `.zeph/config.md`.

## Embeddable Trait and EmbeddingRegistry

The `Embeddable` trait provides a generic interface for any type that can be embedded in Qdrant. It requires `id()`, `content_for_embedding()`, `content_hash()`, and `to_payload()` methods. `EmbeddingRegistry<T: Embeddable>` is a generic sync/search engine that delta-syncs items by BLAKE3 content hash and performs cosine similarity search. This pattern is used internally by skill matching, MCP tool registry, and code indexing.

## Credential Scrubbing

When `memory.redact_credentials` is enabled (default: `true`), Zeph scrubs credential patterns from message content before sending it to the LLM context pipeline. This prevents accidental leakage of API keys, tokens, and passwords stored in conversation history. The scrubbing runs via `scrub_content()` in the context builder and covers the same patterns as the output redaction system (see [Security — Secret Redaction](../reference/security.md#secret-redaction)).

## Autosave Assistant Responses

By default, only user messages generate vector embeddings. Enable `autosave_assistant` to persist assistant responses to SQLite and optionally embed them for semantic recall:

```toml
[memory]
autosave_assistant = true    # Save assistant messages (default: false)
autosave_min_length = 20     # Minimum content length for embedding (default: 20)
```

When enabled, assistant responses shorter than `autosave_min_length` are saved to SQLite without generating an embedding (via `save_only()`). Responses meeting the threshold go through the full embedding pipeline. User messages always generate embeddings regardless of this setting.

## Memory Snapshots

Export and import conversation history as portable JSON files for backup, migration, or sharing between instances.

```bash
# Export all conversations, messages, and summaries
zeph memory export backup.json

# Import into another instance (duplicates are skipped)
zeph memory import backup.json
```

The snapshot format (version 1) includes conversations, messages with multipart content, and summaries. Import uses `INSERT OR IGNORE` semantics — existing messages with matching IDs are skipped, so importing the same file twice is safe.

## LLM Response Cache

Cache identical LLM requests to avoid redundant API calls. The cache is SQLite-backed, keyed by a blake3 hash of the message history and model name.

```toml
[llm]
response_cache_enabled = true   # Enable response caching (default: false)
response_cache_ttl_secs = 3600  # Cache entry lifetime in seconds (default: 3600)

[memory]
response_cache_cleanup_interval_secs = 3600  # Interval for purging expired cache entries (default: 3600)
```

A periodic background task purges expired entries. The cleanup interval is configurable via `[memory] response_cache_cleanup_interval_secs` (default: 3600 seconds). Streaming responses bypass the cache entirely — only non-streaming completions are cached.

## Native Memory Tools

When a memory backend is configured, Zeph registers two native tools that the model can invoke explicitly during a conversation, in addition to automatic recall that runs at context-build time.

### `memory_search`

Searches long-term memory across three sources and returns a combined markdown result:

- **Semantic recall** — vector similarity search against past messages (same as automatic recall)
- **Key facts** — structured facts extracted and stored via `memory_save`
- **Session summaries** — summaries from other conversations, excluding the current session

The model invokes this tool when it needs to actively retrieve information rather than rely on what was injected automatically. Example: the user asks "what was the API key format we agreed on last week?" and the model has no relevant context in the current window.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `query` | string (required) | Natural language search query |
| `limit` | integer (optional, default 5) | Maximum number of results per source |

### `memory_save`

Persists content to long-term memory as a key fact, making it retrievable in future sessions.

The model uses this when it identifies information worth preserving explicitly — decisions, preferences, or facts the user stated that should survive context compaction. Content is validated (non-empty, max 4096 characters) before being stored via `remember()`.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `content` | string (required) | The information to persist (max 4096 characters) |

### Registration

`MemoryToolExecutor` is registered in the tool chain only when a memory backend is configured. If `[memory]` is absent or `[memory.semantic]` is disabled, neither tool appears in the model's tool list.

## Query-Aware Memory Routing

By default, semantic recall queries both SQLite FTS5 (keyword) and Qdrant (vector) backends and merges results via reciprocal rank fusion. Query-aware routing selects the optimal backend(s) per query, avoiding unnecessary work.

```toml
[memory.routing]
strategy = "heuristic"   # Currently the only strategy (default)
```

The heuristic router classifies queries into three routes:

| Route | Backend | When |
|-------|---------|------|
| Keyword | SQLite FTS5 | Code patterns (`::`, `/`), snake_case identifiers, short queries (<=3 words) |
| Semantic | Qdrant vectors | Question words (`what`, `how`, `why`, ...), long natural language (>=6 words) |
| Hybrid | Both + RRF merge | Medium-length queries without clear signals (4-5 words, no question word) |

Question words override code pattern heuristics: `"how does error_handling work"` routes Semantic, not Keyword.

The agent calls `recall_routed()` on `SemanticMemory`, which delegates to the configured router before querying. When Qdrant is unavailable, Semantic-route queries return empty results; Hybrid-route queries fall back to FTS5 only.

## Active Context Compression

Zeph supports two compression strategies for managing context growth:

```toml
[memory.compression]
strategy = "reactive"    # Default — compress only when reactive compaction fires
```

**Reactive** (default) relies on the existing two-tier compaction pipeline (Tier 1 tool output pruning, Tier 2 chunked LLM compaction). No additional configuration needed.

**Proactive** fires compression before reactive compaction when the current token count exceeds `threshold_tokens`:

```toml
[memory.compression]
strategy = "proactive"
threshold_tokens = 80000       # Fire when context exceeds this token count (>= 1000)
max_summary_tokens = 4000      # Cap for the compressed summary (>= 128)
# model = ""                   # Reserved for future per-compression model selection (currently unused)
```

Proactive and reactive compression are mutually exclusive per turn: if proactive compression fires, reactive compaction is skipped for that turn (and vice versa). The `compacted_this_turn` flag resets at the start of each turn.

Proactive compression emits two metrics: `compression_events` (count) and `compression_tokens_saved` (cumulative tokens freed).

> **Note:** Validation rejects `threshold_tokens < 1000` and `max_summary_tokens < 128` at startup.

## Deep Dives

- [Set Up Semantic Memory](../guides/semantic-memory.md) — Qdrant setup guide
- [Context Engineering](../advanced/context.md) — budget allocation, compaction, recall tuning
