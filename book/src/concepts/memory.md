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

### Semantic Response Caching

In addition to exact-match caching, Zeph supports embedding-based similarity matching for cache lookups. When `semantic_cache_enabled = true`, the system embeds incoming message context and searches for cached responses with cosine similarity above `semantic_cache_threshold` (default: 0.95). This allows cache hits even when messages are paraphrased or slightly different.

```toml
[llm]
response_cache_enabled = true
semantic_cache_enabled = true          # Enable semantic similarity matching (default: false)
semantic_cache_threshold = 0.95        # Cosine similarity threshold for cache hit (default: 0.95)
semantic_cache_max_candidates = 10     # Max entries to examine per lookup (default: 10)
```

The threshold controls the tradeoff between hit rate and relevance: lower values (0.92) produce more hits but risk returning less relevant cached responses; higher values (0.98) are more conservative. `semantic_cache_max_candidates` controls how many entries are examined per query — increase to 50+ for better recall at the cost of latency.

## Write-Time Importance Scoring

When `importance_enabled = true`, each message receives an importance score (0.0-1.0) at write time. The score is computed by an LLM classifier that evaluates how decision-relevant the message content is. During semantic recall, the importance score is blended with the similarity score using `importance_weight` (default: 0.15), boosting recall of architecturally significant decisions and key facts.

```toml
[memory.semantic]
importance_enabled = true         # Enable write-time importance scoring (default: false)
importance_weight = 0.15          # Blend weight for importance in recall ranking (default: 0.15)
```

The weight controls how much importance influences the final recall ranking: `0.0` disables importance entirely (pure similarity), `1.0` makes importance the dominant signal. The default `0.15` provides a subtle boost to high-importance messages without disrupting similarity-based ranking.

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

## Adaptive Memory Admission Control (A-MAC)

By default, every message that crosses the minimum length threshold is embedded and stored in the vector backend. A-MAC adds a learned gate that evaluates each candidate message against the current memory state before committing the write. Only messages that are sufficiently novel — dissimilar to recently stored content — are admitted, preventing the vector index from filling with near-duplicate information.

A-MAC is disabled by default. Enable it in `[memory.admission]`:

```toml
[memory.admission]
enabled = true
threshold = 0.40            # Composite score threshold; messages below this are rejected (default: 0.40)
fast_path_margin = 0.15     # Skip full check and admit immediately when score >= threshold + margin (default: 0.15)
admission_provider = "fast" # Provider name for LLM-assisted admission decisions (optional)

[memory.admission.weights]
future_utility = 0.30       # LLM-estimated future reuse probability (heuristic mode only)
factual_confidence = 0.15   # Inverse of hedging markers (e.g. "I think", "maybe")
semantic_novelty = 0.30     # 1 - max similarity to existing memories
temporal_recency = 0.10     # Always 1.0 at write time
content_type_prior = 0.15   # Role-based prior (user messages score higher)
```

The `fast_path_margin` short-circuits the admission check for clearly novel messages, reducing embedding lookups on low-similarity content. When `admission_provider` is set, borderline cases (similarity near `threshold`) are escalated to an LLM for a binary admit/reject decision; without it, the threshold comparison is the sole gate.

## ClawVM Typed Pages and MemReader Quality Gate

Context compaction produces pages of different types — tool outputs, conversation turns, memory excerpts, system context — each with distinct fidelity requirements. ClawVM (Compact Low-Alignment View Machine) classifies every compacted page into a `PageType` enum and enforces per-type `PageInvariant` traits at compaction boundaries. This ensures that tool outputs preserve call/result pairs, conversation turns preserve multi-part messages, and memory excerpts preserve citations.

**Page types:**

| Type | Content | Invariant |
|------|---------|-----------|
| `ToolOutput` | Single tool result | No orphaned ToolUse/ToolResult pairs |
| `ConversationTurn` | User or assistant message | Multipart structure intact (text, tool calls, etc.) |
| `MemoryExcerpt` | Recalled or injected memory | Citation completeness, no dangling references |
| `SystemContext` | Project context, instructions | No truncation of logical sections |

When a page is compacted, Zeph appends an audit record to a bounded async sink, allowing external systems to verify that invariants were enforced.

MemReader quality gate scores candidate memories on three dimensions before admitting them into the vector store:

1. **Information value** — cosine similarity vs. recent context (avoid duplicates)
2. **Reference completeness** — pronoun/deictic heuristic (is meaning clear without context?)
3. **Contradiction risk** — graph edge conflicts (does it contradict known facts?)

The gate is fail-open: if embedding, LLM, or graph queries error out, neutral defaults are used and the message is admitted. Enable it in `[memory.quality_gate]`:

```toml
[memory.quality_gate]
enabled = true
information_value_threshold = 0.3       # Skip admission if similarity exceeds this
reference_completeness_threshold = 0.5  # Require non-empty pronouns in content
contradiction_risk_threshold = 0.7      # Flag if graph edges show conflict
```

Quality gate operates downstream of A-MAC admission, making both gates independent and composable.

### RL-Based Admission Strategy

The default `heuristic` strategy uses static weights and an optional LLM call for the `future_utility` factor. The `rl` strategy replaces the `future_utility` LLM call with a trained logistic regression model that learns from actual recall outcomes.

The RL model collects `(query, content, was_recalled)` triples from every admitted and rejected message over time. When the training corpus reaches `rl_min_samples`, the model is trained and deployed. Below that threshold the system automatically falls back to `heuristic`.

```toml
[memory.admission]
enabled = true
admission_strategy = "rl"          # "heuristic" (default) or "rl"
rl_min_samples = 500               # Training samples required before RL activates (default: 500)
rl_retrain_interval_secs = 3600    # Background retraining interval in seconds (default: 3600)
```

> [!WARNING]
> `admission_strategy = "rl"` is currently a preview feature. The model infrastructure is wired and sample collection is active, but the trained model is not yet connected to the admission path — the system will emit a startup warning and fall back to `heuristic`. Full RL-gated admission is tracked in [#2416](https://github.com/bug-ops/zeph/issues/2416).

> [!NOTE]
> Migration 055 adds the tables required for RL sample storage. Run `zeph --migrate-config` when upgrading an existing installation.

## MemScene Consolidation

MemScene groups semantically related messages into *scenes* — short-lived narrative units covering a coherent sub-topic within a session. Scenes are detected automatically in the background and consolidated into a single embedding before the individual messages are demoted in the recall index. This compresses the vector space without discarding information: a scene embedding captures the collective meaning of its member messages, and scene summaries are searchable in future sessions.

MemScene is configured under `[memory.tiers]`:

```toml
[memory.tiers]
scene_enabled = true
scene_similarity_threshold = 0.80  # Minimum cosine similarity for messages to be grouped into the same scene (default: 0.80)
scene_batch_size = 10              # Number of messages to evaluate per consolidation cycle (default: 10)
scene_provider = "fast"            # Provider name for scene summary generation
```

`scene_provider` must reference a `[[llm.providers]]` entry. If unset, the default provider is used. Scenes are stored in SQLite alongside their member message IDs and can be inspected with `zeph memory stats`.

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

> [!NOTE]
> Validation rejects `threshold_tokens < 1000` and `max_summary_tokens < 128` at startup.

### Tool Output Archive (Memex)

When `archive_tool_outputs = true`, Zeph saves the full body of every tool output in the compaction range to SQLite before summarization begins. The archived entries are stored in the `tool_overflow` table with `archive_type = 'archive'` and are excluded from the normal overflow cleanup pass.

During compaction the LLM sees placeholder messages instead of the full outputs, keeping the summarization prompt small. After the LLM produces its summary, Zeph appends UUID reference lines (one per archived output) to the summary text. This gives you a complete audit trail of tool outputs that survived context compaction.

This feature is disabled by default because it increases SQLite storage usage. Enable it when you need durable tool output history across long sessions:

```toml
[memory.compression]
archive_tool_outputs = true
```

> [!TIP]
> Tool output archives are written by database migration 054. Run `zeph --migrate-config` if you are upgrading an existing installation.

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

> [!NOTE]
> Guidelines are injected only when `enabled = true` and at least one guidelines version exists in SQLite. The guidelines document grows incrementally as the agent accumulates failure experience.

### Per-Category Compression Guidelines

By default a single global guidelines document is maintained for the entire conversation. When `categorized_guidelines = true`, the updater maintains **four independent documents** — one per content category — and injects only the relevant document during compaction:

| Category | Content covered |
|----------|----------------|
| `tool_output` | Tool call results, shell output, file reads |
| `assistant_reasoning` | Agent reasoning steps and explanations |
| `user_context` | User instructions, preferences, and goals |
| `unknown` | Messages that do not match a category |

Each category runs its own update cycle: a category is updated only when its unprocessed failure pair count reaches `update_threshold`, avoiding unnecessary LLM calls for categories that have few failures.

Enable per-category guidelines alongside the base feature:

```toml
[memory.compression_guidelines]
enabled = true
categorized_guidelines = true    # Maintain separate guidelines per content category (default: false)
update_threshold = 5
```

> [!TIP]
> Per-category guidelines reduce the chance that tool-output compression rules interfere with how assistant reasoning is compressed, and vice versa. Enable this when you have long sessions mixing heavy tool use with extended reasoning chains.

## Graph Memory

With the `graph-memory` feature enabled, Zeph extracts entities and relationships from conversations and stores them as a knowledge graph in SQLite. This enables multi-hop reasoning ("how is X related to Y?"), temporal fact tracking ("user switched from vim to neovim"), and cross-session entity linking.

Graph memory is opt-in and complementary to vector + keyword search. After each user message, a background task extracts entities and edges via LLM. On subsequent turns, matched graph facts are injected into the context as a system message alongside recalled messages. The context budget allocates 4% of available tokens to graph facts (taken proportionally from summaries, semantic recall, cross-session, and code context allocations). Messages flagged with injection patterns skip extraction for security.

```toml
[memory.graph]
enabled = true
max_hops = 2
recall_limit = 10
```

### Hebbian Reinforcement

Hebbian updates strengthen edge weights in the graph when facts are recalled. After retrieving a fact from the graph, the edges traversed during retrieval are incremented by a configurable learning rate, making frequently-used relationships stronger over time.

```toml
[memory.hebbian]
enabled = false                    # disabled by default; opt-in
hebbian_lr = 0.1                   # learning rate for weight increment
```

When enabled, the system records every graph retrieval and applies weight updates fire-and-forget in the background.

### HeLa-Mem Spreading Activation Retrieval

HeLa-Mem (Hebbian-Latent Memory) extends graph retrieval with spreading activation: starting from the top-1 ANN anchor node, the system performs breadth-first search through the graph, propagating edge weights (`path_weight = Π edge.weight`). Each visited node is scored as `path_weight × cosine(query, entity)`, with negative cosine clamped to 0.0. Multi-path convergence keeps the maximum `path_weight`.

```toml
[memory.hebbian]
spreading_activation = true        # enable spreading activation (default: false)
spread_depth = 3                   # BFS depth limit (default: 2)
spread_edge_types = ["related_to", "contradicts"]  # filter edges by type (empty = all)
step_budget_ms = 8                 # per-step timeout in milliseconds (default: 8)
```

An 8 ms circuit breaker emits a WARN log and returns empty results on budget exhaustion. Isolated anchors (no outgoing edges) fall back to a synthetic `HelaFact` scored by the real anchor cosine.

See [Graph Memory](graph-memory.md) for the full concept guide.

## ReasoningBank — Distilled Reasoning Strategy Memory

After each assistant turn, a three-stage pipeline (self-judge → distillation → store) extracts reasoning strategies and stores them as a new kind of memory. A ≤3-sentence strategy summary captures how the agent solved a problem and can be retrieved in future turns.

```toml
[memory.reasoning]
enabled = false                   # disabled by default; opt-in
store_limit = 100                 # max entries in reasoning_strategies table (default: 100)
self_judge_window = 2             # messages to evaluate (default: 2 = final user+assistant exchange)
min_assistant_chars = 50          # skip trivial responses shorter than this (default: 50)
```

Strategies are stored in SQLite and retrieved at context-build time by embedding similarity. The system maintains an LRU eviction with hot-row protection: frequently-used strategies are kept even under eviction pressure.

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

## Structured Anchored Summarization

When hard compaction fires, the summarizer can produce structured summaries anchored to specific information categories. The `AnchoredSummary` format replaces free-form prose with five mandatory sections:

1. **Session Intent** — what the user is trying to accomplish
2. **Files Modified** — file paths, function names, structs referenced
3. **Decisions Made** — architectural or implementation decisions with rationale
4. **Open Questions** — unresolved items or ambiguities
5. **Next Steps** — concrete actions to take immediately

Anchored summaries are validated for completeness (`session_intent` and `next_steps` must be non-empty) and rendered as Markdown with `[anchored summary]` headers for context injection. This structured format reduces information loss during compaction compared to unstructured prose summaries.

## SleepGate Forgetting Pass

Over time, the vector index accumulates stale or low-value embeddings that dilute recall quality. SleepGate implements a periodic forgetting pass inspired by memory consolidation during sleep: it scans stored embeddings, scores them on recency, access frequency, and semantic density, then soft-deletes entries below the retention threshold.

```toml
[memory.forgetting]
enabled = true
interval_secs = 86400          # Run forgetting pass every N seconds (default: 86400 = 24h)
retention_threshold = 0.30     # Composite score below which entries are forgotten (default: 0.30)
```

Forgotten entries are soft-deleted (marked in SQLite, removed from the vector index) and can be restored manually if needed.

## Multi-Vector Chunking

Long messages (tool outputs, code blocks, large paste operations) that exceed the embedding model's token limit are automatically split into overlapping chunks, each embedded independently. During recall, chunk scores are aggregated back to the parent message using max-pooling, so a message is retrieved if any of its chunks is relevant.

This runs in the real-time embedding path — no configuration is needed. The chunk size and overlap are derived from the embedding model's context window.

## BATS Budget Hint

The Budget-Aware Token Steering (BATS) system injects a budget hint into the system prompt that tells the LLM how much context space remains. This helps the model produce appropriately-sized responses and make better decisions about when to use tools versus answering from context.

BATS also implements a utility-based 5-way action policy that evaluates each agent turn against five action categories (respond, search, tool-use, delegate, wait) and selects the action with the highest expected utility given the current context budget and conversation state.

## Cost-Sensitive Store Routing

When multiple storage backends are available (SQLite vectors, Qdrant, graph store), the memory system routes write operations to the backend with the lowest cost for the given content type. Short factual statements are routed to the graph store, long narratives to vector storage, and structured data to SQLite key-value pairs.

```toml
[memory.routing]
cost_sensitive = true          # Enable cost-aware write routing (default: false)
```

## Goal-Conditioned Write Gate

When enabled, the write gate evaluates whether a candidate memory entry is relevant to the user's current goal before admitting it. This prevents the memory system from storing tangential information during long exploratory sessions.

The goal text is extracted from the most recent `/plan` goal or from the first user message in the session if no plan is active.

## Kumiho Belief Revision

Kumiho implements belief revision for the graph memory store. When new information contradicts an existing entity-relationship fact, Kumiho evaluates the conflict using temporal recency and source reliability, then either updates the existing edge, creates a versioned override, or flags the conflict for user resolution.

This is paired with D-MEM RPE (Reward Prediction Error) routing for graph memory, which uses prediction errors from graph queries to adaptively weight the graph store's contribution to hybrid recall.

## Persona Memory

Persona memory extracts persistent user-preference and domain-knowledge facts from conversation history. Extracted facts are injected into context at assembly time, giving the agent a stable model of user expertise, goals, and preferences across sessions.

Facts are extracted by a fast LLM provider after the session accumulates enough user messages (controlled by `min_messages`). A self-referential heuristic gate skips extraction for agent-to-agent sessions. When conflicting facts are detected, the newer entry marks the older one via `supersedes_id`, preserving history without duplication.

```toml
[memory.persona]
enabled                 = false
persona_provider        = "fast"   # cheap extraction model; falls back to primary
min_confidence          = 0.6      # facts below this threshold are discarded
min_messages            = 3        # minimum user messages before first extraction
max_messages            = 10       # messages fed to LLM per extraction pass
extraction_timeout_secs = 10
context_budget_tokens   = 500
```

## Key Facts Semantic Dedup

When storing key facts via `memory_save`, Zeph can skip near-duplicate entries that are already present in the Qdrant collection. Before each insert, the new fact's embedding is compared to the nearest neighbour in `zeph_key_facts`. If the cosine similarity is at or above `key_facts_dedup_threshold`, the fact is silently discarded. This prevents the key-facts collection from accumulating paraphrased versions of the same information.

The check is fail-open: if the similarity search returns an error, the fact is stored rather than dropped.

```toml
[memory]
key_facts_dedup_threshold = 0.95   # Cosine similarity above which a near-duplicate is suppressed (default: 0.95)
```

## Trajectory Memory

Trajectory memory captures procedural ("how to do X") and episodic ("what happened in turn N") entries from tool-call turns. Procedural entries are injected as "past experience" during context assembly, helping the agent reuse successful tool patterns across sessions.

Extraction runs after every turn that contains tool calls, using a fast LLM provider to classify and summarise each tool sequence. Only entries above `min_confidence` are stored.

```toml
[memory.trajectory]
enabled                 = false
trajectory_provider     = "fast"   # cheap extraction model; falls back to primary
context_budget_tokens   = 400      # token budget for trajectory hints in context
recall_top_k            = 5        # procedural entries retrieved per turn
min_confidence          = 0.6
max_messages            = 10
extraction_timeout_secs = 10
```

## Category-Aware Memory

When enabled, messages are tagged with a category derived from the active skill or tool context. The category is stored in the `messages.category` column and used as a payload filter during Qdrant recall, scoping semantic search to the relevant topic area.

```toml
[memory.category]
enabled  = false
auto_tag = true    # derive category from active skill or tool type automatically
```

## TiMem — Temporal-Hierarchical Memory Tree

TiMem organises memories as leaf nodes and periodically consolidates them into hierarchical summaries. Each sweep clusters similar leaves by cosine similarity and asks a fast LLM to produce a parent-level summary. Context assembly uses tree traversal for complex queries, returning a mix of leaf-level detail and higher-level summaries within the token budget.

```toml
[memory.tree]
enabled                = false
consolidation_provider = "fast"  # falls back to primary
sweep_interval_secs    = 300     # background consolidation interval
batch_size             = 20      # leaves processed per sweep
similarity_threshold   = 0.8     # cosine threshold for clustering
max_level              = 3       # maximum tree depth above leaves
context_budget_tokens  = 400
recall_top_k           = 5
min_cluster_size       = 2       # minimum cluster size to trigger LLM consolidation
```

## Time-Based Microcompact

Microcompact clears stale low-value tool outputs from context when the session has been idle longer than `gap_threshold_minutes`. This is a zero-LLM-cost in-memory operation that reduces context pressure before compaction runs.

Cleared tool types: `bash`, `shell`, `grep`, `rg`, `find`, `web_fetch`, `web_search`, `read`, `cat`, `list_directory`. The `keep_recent` most recent entries from these tools are always preserved.

```toml
[memory.microcompact]
enabled               = false
gap_threshold_minutes = 60   # idle gap in minutes before clearing stale outputs
keep_recent           = 3    # most recent low-value tool outputs to preserve
```

## autoDream Background Consolidation

autoDream runs a background memory consolidation sweep after a session ends, once both gates pass: at least `min_sessions` sessions have completed and at least `min_hours` have elapsed since the last consolidation. The sweep merges duplicate memories, updates stale facts, and removes redundant entries.

Gates are in-process only — they reset on restart. The first consolidation always passes the hours gate (no prior timestamp).

```toml
[memory.autodream]
enabled                = false
min_sessions           = 3     # sessions since last consolidation
min_hours              = 24    # hours since last consolidation
consolidation_provider = ""    # provider name; falls back to primary
max_iterations         = 8     # safety bound for the consolidation sweep
```

## MagicDocs — Auto-Maintained Markdown

MagicDocs detects files containing a `# MAGIC DOC:` header when they are read by file tools, registers them in a per-session list, and periodically rewrites them via a background LLM call to keep them accurate.

Updates run every `min_turns_between_updates` tool-call turns. Only one background update runs at a time; if the previous update is still running the current trigger is skipped. The TUI status bar shows "Updating N magic doc(s)…" while an update is in progress.

To mark a file as auto-maintained, add `# MAGIC DOC: <description>` as the first line.

When MagicDocs is enabled, the file-read tools (`read`, `file_read`, `cat`, `view`, `open`) are automatically added to `utility_scorer.exempt_tools`, bypassing utility scoring so the files are always read and their content reaches the scanner. Any user-configured `exempt_tools` entries are preserved and merged.

```toml
[magic_docs]
enabled                   = false
min_turns_between_updates = 5    # turns between updates for the same file
update_provider           = ""   # provider name; falls back to primary
max_iterations            = 4    # max iterations per update call
```

## Query Bias Correction

First-person queries ("What did I do last week?") are shifted toward the user's profile centroid embedding before vector search. This improves recall of past user-specific decisions and preferences.

```toml
[memory.retrieval]
query_bias_correction = true       # enable bias correction (default: false)
query_bias_profile_weight = 0.25   # blend weight: 0.25 = 25% centroid, 75% query (default: 0.25)
```

The profile centroid is cached with a 300-second TTL in a bounded `RwLock`. Computation failures are non-sticky: the system falls through to the previous cache or disables bias for that turn.

## Store Routing

Store routing classifies each incoming query and routes it to the appropriate memory backend(s), avoiding unnecessary store lookups for simple requests.

```toml
[memory.store_routing]
enabled                      = false
strategy                     = "heuristic"   # "heuristic" | "llm" | "hybrid"
routing_classifier_provider  = ""            # provider name; falls back to primary
fallback_route               = "hybrid"      # route used when confidence < threshold
confidence_threshold         = 0.7
```

| Strategy | Behavior |
|----------|----------|
| `heuristic` | Pure pattern matching — zero LLM calls. Fastest and cheapest. Default. |
| `llm` | A lightweight LLM classifies the query intent and selects the target store. Higher accuracy on ambiguous queries; adds one LLM call per turn. |
| `hybrid` | Heuristic runs first. When confidence is below `confidence_threshold`, the decision escalates to the LLM. Balances cost and accuracy. |

`routing_classifier_provider` should reference a cheap/fast provider (e.g., `gpt-4o-mini`) declared in `[[llm.providers]]`. Leave it empty to fall back to the primary provider.

`fallback_route` is the store used when the classifier cannot reach a confident decision (applies to `hybrid` strategy). The default value `"hybrid"` sends the query to all stores.

Store routing is disabled by default (`enabled = false`). When disabled, `HeuristicRouter` is used directly, which is equivalent to `strategy = "heuristic"` with routing always enabled.

## Memory Tiers

The tier promotion system organises memories into a hierarchy of four conceptual tiers:

| Tier | Description |
|------|-------------|
| **Working memory** | Active conversation messages in the current session |
| **Episodic** | Recent messages persisted to SQLite after the turn completes |
| **Semantic** | Frequently-recalled facts promoted from episodic by the background sweep |
| **Archival** | Long-term storage; entries demoted from semantic when they age out of active recall |

Promotion is driven by a background sweep that clusters near-duplicate episodic messages by cosine similarity. When a fact appears in at least `promotion_min_sessions` distinct sessions, the cluster is distilled into a single semantic-tier entry via an LLM call, and the source episodic entries are marked `agent_visible = false`.

The tier system is disabled by default. Enable it under `[memory.tiers]`:

```toml
[memory.tiers]
enabled                  = true
promotion_min_sessions   = 3      # distinct sessions a fact must appear in before promotion (>= 2)
similarity_threshold     = 0.92   # cosine similarity threshold for clustering episodic duplicates
sweep_interval_secs      = 3600   # how often the background sweep runs (seconds)
sweep_batch_size         = 100    # messages evaluated per sweep cycle (>= 1)
```

### MemScene Consolidation

MemScene is a second-pass sweep that consolidates groups of semantically related *semantic-tier* messages into scene-level summaries. A scene covers a coherent sub-topic: its embedding captures the collective meaning of its member messages, compressing the vector space without discarding information. Scene summaries are indexed and searchable in future sessions.

MemScene is configured alongside the tier system:

```toml
[memory.tiers]
enabled                       = true
scene_enabled                 = true
scene_similarity_threshold    = 0.80   # cosine similarity threshold for scene grouping (in [0.5, 1.0])
scene_batch_size              = 50     # unassigned semantic messages processed per sweep (>= 1)
scene_provider                = "fast" # [[llm.providers]] name for scene label/summary generation
scene_sweep_interval_secs     = 7200   # how often the scene consolidation sweep runs (seconds)
```

`scene_provider` must reference a `[[llm.providers]]` entry. When unset, the default provider is used. Scenes are stored in SQLite alongside their member message IDs and can be inspected with `zeph memory stats`.

> [!NOTE]
> `scene_similarity_threshold` is validated to be in `[0.5, 1.0]` and `scene_batch_size` must be `>= 1`. Invalid values are rejected at startup.

## Next Steps

- [Set Up Semantic Memory](../guides/semantic-memory.md) — Qdrant setup guide
- [Context Budgets](context-budgets.md) — BATS budget hints and allocation strategy
- [SleepGate](../advanced/sleep-gate.md) — automatic memory forgetting and index hygiene
- [Graph Memory](graph-memory.md) — entity-relationship tracking and multi-hop reasoning
- [Context Engineering](../advanced/context.md) — budget allocation, compaction, recall tuning
