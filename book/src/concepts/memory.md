# Memory and Context

Zeph uses a dual-store memory system: SQLite for structured conversation history and a configurable vector backend (Qdrant or embedded SQLite) for semantic search across past sessions.

## Conversation History

All messages are stored in SQLite. The CLI channel provides persistent input history with arrow-key navigation, prefix search, and Emacs keybindings. History persists across restarts.

When conversations grow long, Zeph compacts history automatically using a two-tier strategy. The soft tier fires at `soft_compaction_threshold` (default 0.70): it prunes tool outputs and applies pre-computed deferred summaries without an LLM call. The hard tier fires at `hard_compaction_threshold` (default 0.90): it runs full LLM-based chunked compaction. Compaction uses dual-visibility flags on each message: original messages are marked `agent_visible=false` (hidden from the LLM) while remaining `user_visible=true` (preserved in UI). A summary is inserted as `agent_visible=true, user_visible=false` — visible to the LLM but hidden from the user. This is performed atomically via `replace_conversation()` in SQLite. The result: the user retains full scroll-back history while the LLM operates on a compact context.

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

## Cross-Session History Restore

When a session is resumed, Zeph restores previous message history from SQLite. The restore pipeline applies `sanitize_tool_pairs()` to ensure every `ToolUse` message has a matching `ToolResult`. Orphaned `ToolUse` or `ToolResult` parts at session boundaries — caused by session interruptions or compaction boundary splits — are detected and stripped before the history reaches the LLM. This prevents Claude API 400 errors that occur when the API receives unmatched tool call pairs.

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
| Graph | Graph store + Hybrid fallback | Relationship patterns (`related to`, `opinion on`, `connection between`, `know about`). Requires `graph-memory` feature; falls back to Hybrid when disabled |

Question words override code pattern heuristics: `"how does error_handling work"` routes Semantic, not Keyword. Relationship patterns take priority over all other heuristics: `"how is Rust related to this project"` routes Graph, not Semantic.

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

## Failure-Driven Compression Guidelines

When `[memory.compression_guidelines]` is enabled, the agent learns from its own compaction mistakes. After each hard compaction, it watches the next several LLM responses for a two-signal context-loss indicator: an uncertainty phrase (e.g. "I don't recall", "I'm not sure if") combined with a prior-context reference (e.g. "earlier you mentioned", "we discussed before"). When both signals appear together in the same response, the pair is recorded as a compression failure in SQLite.

A background updater wakes on a configurable interval, and when the number of unprocessed failure pairs exceeds `update_threshold`, it calls the LLM to synthesize updated compression guidelines. The resulting guidelines are sanitized to strip prompt-injection attempts and stored in SQLite. Every subsequent compaction prompt includes the active guidelines inside a `<compression-guidelines>` block, steering the summarizer to preserve categories of information that were lost before.

The feature is disabled by default:

```toml
[memory.compression_guidelines]
enabled = true
update_threshold = 5             # Minimum failure pairs before triggering an update (default: 5)
max_guidelines_tokens = 500      # Token budget for the guidelines document (default: 500)
max_pairs_per_update = 10        # Failure pairs consumed per update cycle (default: 10)
detection_window_turns = 10      # Turns after hard compaction to watch for context loss (default: 10)
update_interval_secs = 300       # Seconds between background updater checks (default: 300)
max_stored_pairs = 100           # Maximum unused failure pairs retained (default: 100)
```

> **Note:** Guidelines are injected only when `enabled = true` and at least one guidelines version exists in SQLite. The guidelines document grows incrementally as the agent accumulates failure experience.

## Graph Memory

With the `graph-memory` feature enabled, Zeph extracts entities and relationships from conversations and stores them as a knowledge graph in SQLite. This enables multi-hop reasoning ("how is X related to Y?"), temporal fact tracking ("user switched from vim to neovim"), and cross-session entity linking.

Graph memory is opt-in and complementary to vector + keyword search. After each user message, a background task extracts entities and edges via LLM. On subsequent turns, matched graph facts are injected into the context as a system message alongside recalled messages. The context budget allocates 4% of available tokens to graph facts (taken proportionally from summaries, semantic recall, cross-session, and code context allocations). Messages flagged with injection patterns skip extraction for security.

```toml
[memory.graph]
enabled = true
max_hops = 2
recall_limit = 10
```

See [Graph Memory](graph-memory.md) for the full concept guide.

## Session Summary on Shutdown

When a session ends (graceful shutdown), Zeph checks whether a session summary already exists
for the conversation. If none does — which is typical for short or interrupted sessions that
never triggered hard compaction — it generates a lightweight LLM summary of the recent messages
and stores it in the `zeph_session_summaries` vector collection. This makes the session
retrievable by `search_session_summaries` in future conversations, enabling cross-session recall
even for brief interactions.

The guard is SQLite-authoritative: if a summary record exists in SQLite (written by either the
shutdown path or a previous hard compaction), the shutdown path is skipped. This handles the edge
case where a Qdrant write failed but the SQLite record succeeded.

```toml
[memory]
shutdown_summary = true              # default: true
shutdown_summary_min_messages = 4   # skip sessions with fewer user turns
shutdown_summary_max_messages = 20  # cap LLM input to the last N messages
```

The LLM call is bounded by a 5-second timeout (10 seconds worst-case if the structured output
call times out and falls back to plain text). Errors are logged as warnings and never propagate
to the caller — shutdown completes regardless.

## Deep Dives

- [Set Up Semantic Memory](../guides/semantic-memory.md) — Qdrant setup guide
- [Graph Memory](graph-memory.md) — entity-relationship tracking and multi-hop reasoning
- [Context Engineering](../advanced/context.md) — budget allocation, compaction, recall tuning
