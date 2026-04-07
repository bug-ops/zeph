# Spec: Memory System
## Sources
### External- **A-MEM** (NeurIPS 2025) — agentic write-time memory linking: https://arxiv.org/abs/2502.12110
- **Zep: Temporal Knowledge Graph** (Jan 2025) — `valid_from`/`valid_until` edges, LongMemEval +18.5%: https://arxiv.org/abs/2501.13956
- **TA-Mem** (Mar 2026) — adaptive retrieval dispatch by query type, HeuristicRouter: https://arxiv.org/abs/2603.09297
- **Episodic-to-Semantic Memory Promotion** (Jan 2025): https://arxiv.org/pdf/2501.11739 · https://arxiv.org/abs/2512.13564
- **MAGMA** (Jan 2026) — multi-graph agent memory, 0.70 on LoCoMo: https://arxiv.org/abs/2601.03236
- **Context Engineering in Manus** (Oct 2025) — tool output reference pattern: https://rlancemartin.github.io/2025/10/15/manus/
- **Structured Anchored Summarization** (Factory.ai, 2025) — typed summary schemas: https://factory.ai/news/compressing-context

### Internal| File | Contents |
|---|---|
| `crates/zeph-memory/src/semantic/mod.rs` | `SemanticMemory`, recall pipeline, compaction |
| `crates/zeph-memory/src/graph/mod.rs` | Graph memory integration |
| `crates/zeph-llm/src/provider.rs` | `MessagePart`, `MessageMetadata` definitions |
| `crates/zeph-core/src/agent/mod.rs` | `MemoryState`, deferred summary apply logic |

---

`crates/zeph-memory/` — conversation persistence + semantic recall.

## Architecture
```
SemanticMemory (Arc)
├── SqliteStore         — conversation history, message metadata
├── QdrantStore         — vector embeddings for semantic search
├── GraphStore          — entity/edge graph (if graph-memory feature)
└── ResponseCache       — deduplicated LLM response cache
```

## Message Storage
- Every user + assistant turn is persisted to SQLite immediately
- Messages are **never deleted** — only marked with `compacted_at` timestamp or summarized
- `MessageMetadata`: `agent_visible`, `user_visible`, `focus_pinned` — all three fields must be respected
- Conversation is identified by `ConversationId` (UUID); one conversation per agent session

## Tool Pair Summarization (deferred)
1. When a tool call + tool result pair is stored, it is eligible for summarization
2. Summary is computed **lazily** — stored as `deferred_summary` on the message, NOT applied immediately
3. Application is triggered at soft context threshold (~60% used)
4. `apply_deferred_summaries()` must be called before context assembly — never skip
5. Applied summaries are stored as `MessagePart::ToolOutput { compacted_at: Some(ts) }`

## Semantic Recall
Three recall sources injected into each turn (in order):

1. **Semantic recall** — Qdrant cosine similarity search on conversation embeddings
   - Uses MMR (Maximal Marginal Relevance) re-ranking to reduce redundancy
   - Temporal decay: older memories scored lower (Ebbinghaus-inspired)
2. **Code context** — AST-indexed code snippets from `zeph-index` (if `index` feature enabled)
3. **Graph facts** — BFS traversal results from graph memory (if `graph-memory` feature enabled)

Recall results are injected as `MessagePart::Recall`, `MessagePart::CodeContext`, `MessagePart::CrossSession`.

## Compaction Pipeline
Triggered at hard context threshold (~90%):

1. Identify oldest unprotected messages (not `focus_pinned`, not thinking blocks)
2. Batch-summarize with LLM into `MessagePart::Compaction { summary }`
3. Remove original messages from in-memory `messages` vector (they remain in SQLite)
4. Eviction policy: Ebbinghaus forgetting curve (retention score based on recency + access frequency)

## Autosave / Snapshot
- Periodic autosave: snapshot current conversation state to SQLite
- On restart: load last conversation via `--resume` or auto-detect latest session
- `ConversationId` ties SQLite rows to Qdrant point UUIDs (deterministic UUIDv5)

## Key Invariants
- `SemanticMemory` is always `Arc<>` — shared between agent loop and background tasks
- SQLite and Qdrant must stay consistent — write to both or neither
- Deferred summaries must be applied before context assembly — never build context with unapplied summaries
- `focus_pinned` messages are never evicted or compacted
- Recall source order is fixed: semantic → code → graph

---

## Semantic Response Caching
`crates/zeph-memory/src/response_cache.rs` — dual-mode response cache: exact-match and embedding-based similarity.

### Overview
`ResponseCache` stores LLM responses in SQLite with TTL and supports both key-based and semantic (embedding) lookup. The semantic path uses cosine similarity between the stored and query embeddings to find responses to semantically equivalent prompts without full LLM inference.

### Lookup Modes
**Exact match** (`get(key)` / `put(key, response, model)`):
- BLAKE3 hash of `(last_user_message, model)` used as cache key
- Key intentionally ignores conversation history — suitable for repeated identical prompts
- Tool-call responses must not be cached via this path

**Semantic match** (`get_semantic()` / `put_with_embedding()`):
- Fetches up to `max_candidates` non-expired rows matching `embedding_model`
- Deserializes embeddings from BLOB using `bytemuck::try_cast_slice` — corrupt BLOBs are skipped with a WARN log, never panicked
- Returns `(response, score)` if best cosine score >= `similarity_threshold`, else `None`
- Dimension mismatch between stored and query embeddings yields score 0.0 (no hit)
- Only the newest entries (by `embedding_ts DESC`) are scanned up to `max_candidates`

### Cache Invalidation
- `invalidate_embeddings_for_model(old_model)`: NULLs embeddings for stale model — exact-match entries survive
- `cleanup(current_model)`: two-phase — DELETE expired rows, NULL stale embeddings (wrapped in single SQLite transaction for atomicity)
- Embedding model change discovered per turn: prevents cross-model false hits

### Config
| Field | TOML | Default |
|---|---|---|
| Semantic lookup enabled | `llm.semantic_cache_enabled` | `false` |
| Similarity threshold | `llm.semantic_cache_threshold` | `0.90` |
| Max candidates scanned | `llm.semantic_cache_max_candidates` | `50` |
| TTL | `llm.response_cache_ttl_secs` | `3600` |

### Key Invariants
- Cache key ignores conversation history — same prompt in different conversations produces the same key
- Semantic search is always filtered by `embedding_model` — cross-model lookups are never permitted
- Corrupt embedding BLOBs (length not multiple of 4) are silently skipped — no panic, no hit
- TTL is capped at 1 year (31,536,000 s) to prevent i64 overflow
- Tool-call responses must be excluded from semantic cache to avoid stale side-effect results
- NEVER use semantic cache for `memory_search`, `memory_save`, `scheduler`, or any write-path tool responses

---

## Structured Anchored Summarization
`crates/zeph-memory/src/anchored_summary.rs` — typed compaction summary schema replacing free-form prose.

### Overview
`AnchoredSummary` is a five-section structured schema used during hard compaction when `[memory] structured_summaries = true`. It replaces the free-form 9-section prose summary with a machine-readable JSON structure, enabling more reliable context injection and debug analysis.

### Schema
| Field | Mandatory | Limit | Purpose |
|---|---|---|---|
| `session_intent` | Yes | 2,000 chars | User's overarching goal for the session |
| `files_modified` | Soft | 50 entries × 500 chars | File paths, function/struct names touched |
| `decisions_made` | Soft | 50 entries × 500 chars | Architecture/implementation decisions with rationale |
| `open_questions` | No | 50 entries × 500 chars | Unresolved ambiguities or blocked items |
| `next_steps` | Yes | 50 entries × 500 chars | Concrete immediate next actions |

`is_complete()` returns false if `session_intent` is blank or `next_steps` is empty. Empty `files_modified` or `decisions_made` triggers a warning log but does not block compaction.

### Rendering
`to_markdown()` renders as `[anchored summary]` Markdown for context injection. Empty optional sections are omitted. Leading `- ` bullet prefixes on entries are stripped to prevent double-bullets.

### Fallback
Structured summarization calls `chat_typed_erased::<AnchoredSummary>()`. On any LLM or validation failure the system falls back to prose summarization — it is never a hard failure.

### Config
```toml
[memory]
structured_summaries = false  # opt-in; default false
```

### Key Invariants
- `session_intent` and `next_steps` are mandatory — incomplete summaries are not committed
- Field length limits enforced by `validate()` before injection — never skip validation
- Structured path uses `chat_typed_erased` only — never construct `AnchoredSummary` from unvalidated LLM output
- Fallback to prose is always available — structured summaries must never block compaction
- NEVER store raw LLM JSON in `summaries.content` without deserialization + validation

---

## Compaction Probe Validation
`crates/zeph-memory/src/compaction_probe.rs` — post-compression context integrity check.

### Overview
The compaction probe validates summary quality before committing it to context. It generates factual questions from the messages being compacted, answers them using only the summary, and scores answers using token-set-ratio similarity. Disabled by default to avoid additional LLM cost.

### Pipeline
1. `generate_probe_questions()`: LLM call generates up to `max_questions` factual Q&A pairs from the compacted messages (tool bodies truncated to 500 chars to focus on decisions, not raw output)
2. `answer_probe_questions()`: Second LLM call answers each question using only the summary text; "UNKNOWN" responses score 0
3. `score_answers()`: Token-set-ratio with substring boost; refusal patterns score 0; average across questions
4. Verdict assignment:
   - `Pass`: score >= `threshold` (default 0.6) — commit summary
   - `SoftFail`: score in `[hard_fail_threshold, threshold)` (default [0.35, 0.6)) — warn, commit
   - `HardFail`: score < `hard_fail_threshold` (default 0.35) — block compaction, return `ProbeRejected`
   - `Error`: transport/timeout failure — proceed without blocking

### Integration
`compact_context()` returns `CompactionOutcome { Compacted | ProbeRejected | NoChange }`. `maybe_compact()` must distinguish `ProbeRejected` from `Compacted` — they must not be merged into one branch.

The entire probe (both LLM calls) is bounded by `timeout_secs` (default 15). Timeout returns `Ok(None)` — treated as no-opinion, compaction proceeds.

### Config
```toml
[memory.compression.probe]
enabled = false
model = ""  # empty = use summary provider
threshold = 0.6
hard_fail_threshold = 0.35
max_questions = 3
timeout_secs = 15
```

### TUI Metrics
Memory panel shows probe rate distribution (`P N% S N% H N% E N%`) and last verdict with color (Pass=green, SoftFail=yellow, HardFail=red, Error=gray). Lines hidden until first probe runs.

### Key Invariants
- Probe is disabled by default — never enable in production without cost analysis
- `hard_fail_threshold` must be strictly less than `threshold` — enforced by `Config::validate()`
- Fewer than 2 questions generated → `Ok(None)` — insufficient statistical power, no verdict
- Probe errors and timeouts are always non-fatal — compaction must not be blocked by probe infrastructure failures
- NEVER treat `ProbeRejected` and `Compacted` as the same outcome in callers
- `max_questions` must be >= 1 and `timeout_secs` >= 1 — enforced by config validation
- Tool body truncation to 500 chars is mandatory to avoid flooding the probe with irrelevant output

---

## Write-Time Importance Scoring
`crates/zeph-memory/src/semantic/importance.rs` — importance scores assigned at memory write time.

### Overview
`compute_importance(content, role)` assigns a score in `[0.0, 1.0]` to each message at write time using three heuristic signals combined as a weighted sum:

| Signal | Weight | Computation |
|---|---|---|
| Marker detection | 50% | 1.0 if `remember:`, `important:`, `always:`, `never forget:`, `key point:`, or `critical:` found at content start or line start (case-insensitive, first 500 chars); 0.0 otherwise |
| Content density | 30% | `x / (1 + x)` where `x = char_count / 300` — sigmoid-like, 300 chars ≈ 0.5, 3,000+ chars ≈ 0.91 |
| Role adjustment | 20% | `user = 0.7`, `assistant = 0.4`, others = 0.5 |

### Integration
- Score stored in `importance_score` column on `messages` table (migration 039)
- Blended into hybrid recall score when `[memory.semantic] importance_enabled = true` (default false, weight 0.15)
- Access counts incremented in batch after each recall turn

### Config
```toml
[memory.semantic]
importance_enabled = false  # opt-in
importance_weight = 0.15    # blend factor [0.0, 1.0]
```

### Key Invariants
- Weights (0.50/0.30/0.20) and threshold constants are fixed — changing them invalidates all stored scores
- Marker check uses safe Unicode char-boundary slicing (`char_byte_boundary`) — never slice raw byte offsets on multi-byte text
- Default score (no marker, short, assistant) ≈ 0.12 — neutral/low importance
- NEVER re-score messages after write — scores are immutable once stored
- NEVER apply importance weighting when `importance_enabled = false`

---

## A-MAC: Adaptive Memory Admission Control
`crates/zeph-memory/src/admission/` (`AdmissionControl`). Implemented in v0.18.0.

### Overview
`AdmissionControl` gates every `remember()` call through a multi-factor scoring
model before writing to SQLite/Qdrant. Low-scoring content is silently dropped,
preventing noise from accumulating in long-term memory. The admission score is
derived from five factors combined as a weighted sum.

### Five-Factor Scoring Model
| Factor | Default Weight | Signal |
|--------|---------------|--------|
| Future utility | 0.30 | LLM-estimated relevance for future retrieval |
| Factual confidence | 0.25 | Probability that content is factually correct (not hallucinated) |
| Semantic novelty | 0.20 | Cosine distance from nearest existing memory |
| Temporal recency | 0.15 | Time decay — recent content scores higher |
| Content-type prior | 0.10 | Prior probability by message type (code > prose > greeting) |

The weighted sum produces a score in `[0.0, 1.0]`. Content with score below
`threshold` is dropped without error.

### Fast Path
When `fast_path_margin > 0.0` (default 0.15), content with score above
`threshold + fast_path_margin` bypasses the LLM utility estimation call to save
cost. The heuristic factors (novelty, recency, type prior) are sufficient for
clearly high-value content.

### API Change: `remember()` Return Type
```rust
// Before:
async fn remember(&self, ...) -> Result<MessageId, MemoryError>

// After :
async fn remember(&self, ...) -> Result<Option<MessageId>, MemoryError>
```

`None` means the content was rejected by admission control. Callers must handle
`None` — it is not an error, it is a policy decision. Do not unwrap the result.

### Config
```toml
[memory.admission]
enabled = false           # opt-in; default off
threshold = 0.30          # admission threshold [0.0, 1.0]
fast_path_margin = 0.15   # score above threshold+margin bypasses LLM call
admission_provider = ""   # provider for utility estimation; empty = primary

[memory.admission.weights]
future_utility = 0.30
factual_confidence = 0.25
semantic_novelty = 0.20
temporal_recency = 0.15
content_type_prior = 0.10
```

### Key Invariants
- `remember()` MUST return `Result<Option<MessageId>>` — callers that rely on always getting a `MessageId` must be updated
- When `enabled = false`, `remember()` always writes (returns `Some(_)`) — no change in behavior
- Admission score is never stored; it is computed per-call and discarded after the gate decision
- Fast path skips only the LLM utility estimation — all other factors still compute
- Weight sum must equal 1.0 at config validation; deviation > 0.01 is a config error
- NEVER block the agent turn on admission scoring failure — fail-open (admit the content)
- NEVER use admission control as a security gate — it is a quality/relevance filter only

---

## MemScene Consolidation
`crates/zeph-memory/src/scene/` — memory scene clustering. Implemented in v0.18.0.

### Overview
MemScene groups semantically related messages into scenes (episodes) using
greedy cosine clustering. Scenes represent discrete episodes of activity (e.g.,
"debugging the websocket handler", "writing unit tests") and improve recall
by treating related messages as a unit.

### Storage Schema
Two new SQLite tables (migration added in v0.18.0):

```sql
-- A scene: a cluster of related messages
CREATE TABLE mem_scenes (
    id         TEXT PRIMARY KEY,
    topic      TEXT NOT NULL,        -- LLM-generated scene label
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    agent_id   TEXT NOT NULL DEFAULT 'default'
);

-- Membership: message → scene
CREATE TABLE mem_scene_members (
    scene_id    TEXT NOT NULL REFERENCES mem_scenes(id),
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    similarity  REAL NOT NULL,       -- cosine similarity to scene centroid
    PRIMARY KEY (scene_id, message_id)
);
```

### Clustering Algorithm
Greedy cosine clustering (single-pass, no k-means):

1. For each unassigned message (with embedding), compute cosine similarity to all existing scene centroids
2. If best similarity >= `scene_similarity_threshold`: assign to that scene, update centroid incrementally
3. Else: create a new scene with this message as the centroid
4. Scene label is generated by LLM call on first creation (batch scheduled, not per-message)

### Config
```toml
[memory.tiers]
scene_enabled = false               # opt-in
scene_similarity_threshold = 0.75   # minimum cosine similarity to join existing scene
scene_batch_size = 32               # messages processed per clustering pass
scene_provider = ""                 # provider for scene labeling; empty = primary
```

### Key Invariants
- Scene clustering is a background operation — never block the agent turn
- Scene membership is soft: a message can belong to at most one scene (lowest similarity wins if conflict)
- Scene centroids are updated incrementally when members are added — never recompute from scratch
- `scene_provider` is used only for LLM-based label generation — clustering itself is embedding-based
- NEVER include `mem_scene_members` rows in context injection directly — scenes are retrieval indices only

---

## Kumiho: AGM Belief Revision for Graph Edges
> **Status**: Implemented. Closes #2441. Migration 056.

`BeliefRevisionConfig` with `similarity_threshold`. `find_superseded_edges()` uses a contradiction heuristic (same relation domain + high cosine similarity = supersession). `superseded_by` column added to `graph_edges` for audit trail.

### Config
```toml
[memory.graph.belief_revision]
enabled = false
similarity_threshold = 0.85
```

### Key Invariants
- Superseded edges are never deleted — `superseded_by` column provides audit trail
- `invalidate_edge_with_supersession()` must set `superseded_by` atomically with the validity flag
- NEVER supersede edges cross-subject — contradiction heuristic requires same relation domain

---

## D-MEM: RPE-Based Tiered Graph Extraction Routing
> **Status**: Implemented. Closes #2442.

`RpeRouter` computes a heuristic surprise score from context similarity and entity novelty. Low-RPE turns skip the MAGMA LLM extraction pipeline. A `consecutive_skips` safety valve forces extraction after `max_skip_turns` consecutive skips.

### Config
```toml
[memory.graph.rpe]
enabled = false
threshold = 0.3
max_skip_turns = 5
```

### Key Invariants
- `consecutive_skips` resets to zero on every extraction (forced or surprise-triggered)
- `max_skip_turns = 0` disables the safety valve — not recommended
- `extract_candidate_entities()` uses regex+keyword detection only (no LLM)

---

## Cost-Sensitive Store Routing
> **Status**: Implemented. Wired into `build_router()` in v0.18.2.

`[memory.store_routing]` config section with strategies `heuristic` / `llm` / `hybrid`. `HybridRouter` runs heuristic first and escalates to LLM only when confidence < threshold. `LlmRouter` uses injection-hardened quoted query.

`StoreRoutingConfig` is now wired into `build_router()` — the router is constructed with the full config at agent startup, making strategy selection effective at boot time. The previous `[memory.routing]` path (`RoutingConfig` / `RoutingStrategy`) is removed.

**Breaking **: `RoutingConfig` and `RoutingStrategy` are removed. Use `[memory.store_routing]` instead of the old `[memory.routing]`.

### Config
```toml
[memory.store_routing]
strategy = "heuristic"              # "heuristic" | "llm" | "hybrid"
routing_classifier_provider = ""    # provider name for LLM routing; empty = primary
confidence_threshold = 0.6          # hybrid: escalate to LLM below this confidence
fallback_route = "sqlite"           # fallback store when routing is uncertain
```

### AsyncMemoryRouter Trait
`AsyncMemoryRouter` is the async routing trait implemented by:
- `HeuristicRouter` — keyword-based, synchronous under the hood
- `HybridRouter` — heuristic + LLM escalation
- `LlmRouter` — pure LLM routing

`recall_routed_async()` dispatches recall to the correct backend via the active router. Callers must use `recall_routed_async()` and not call backend stores directly for routing decisions.

### Key Invariants
- All LLM routing paths fall back to heuristic on failure — NEVER fail-closed on routing
- `HeuristicRouter.route_with_confidence()` returns `1.0 / matched_count` for ambiguous queries
- `--migrate-config` must rewrite `[memory.routing]` → `[memory.store_routing]` for existing configs
- NEVER use `RoutingConfig` or `RoutingStrategy` — they are removed; use `StoreRoutingConfig`

---

## CraniMem: Goal-Conditioned Write Gate
> **Status**: Implemented. Updated v0.18.2: `goal_text` now propagated to A-MAC admission control.

Sixth admission factor `goal_utility` added to `AdmissionFactors` and `AdmissionWeights`. Embedding-first scoring with optional LLM refinement for borderline cases.

In v0.18.2, `goal_text` is explicitly propagated into the admission control call path so that the goal-conditioned write gate has access to the current agent goal at the time of the write decision. When `goal_conditioned_write = true`, a missing or blank `goal_text` skips the goal factor (treated as absent).

### Config
```toml
[memory.admission]
goal_conditioned_write = false
goal_utility_provider = ""
goal_utility_threshold = 0.4
goal_utility_weight = 0.25
```

### Key Invariants
- Goal text < 10 chars is treated as absent (W3.1 fix) — gate does not fire
- Soft floor of 0.1 prevents off-goal memories from scoring absolute zero above threshold
- Zero regression when `goal_conditioned_write = false`
- `goal_text` must be passed through the call chain to `AdmissionControl::evaluate()` — do not read it independently at the gate

---

## RL Admission Control
> **Status**: Implemented. Closes #2416. Migration 055.

`AdmissionStrategy` enum: `heuristic` (default) and `rl`. `admission_training_data` table records all messages seen by A-MAC (admitted and rejected) to eliminate survivorship bias. `was_recalled` flag set by `SemanticMemory::recall()` provides positive training signal. Lightweight logistic regression replaces LLM `future_utility` factor when enough samples are available. Weights persisted in `admission_rl_weights` table.

### Config
```toml
[memory.admission]
admission_strategy = "heuristic"
rl_min_samples = 500
rl_retrain_interval_secs = 3600
```

### Key Invariants
- `rl_min_samples` gate ensures model is not trained on insufficient data
- `was_recalled` must be set by `recall()` — never by the write path
- `admission_training_data` includes both admitted AND rejected entries — survivorship bias is prohibited

---

## Memex: Tool Output Archive
> **Status**: Implemented. Closes #2432. Migration 054.

Before compaction, `ToolOutput` bodies in the compaction range are saved to `tool_overflow` with `archive_type = 'archive'`. Archived UUIDs are appended as postfix after LLM summarization so references survive compaction.

### Config
```toml
[memory.compression]
archive_tool_outputs = false
```

### Key Invariants
- Archive entries (`archive_type = 'archive'`) are excluded from short-lived cleanup
- UUID postfix is appended AFTER LLM summarization — never before

---

## ACON: Per-Category Compression Guidelines
> **Status**: Implemented. Closes #2433. Migration 054.

`compression_failure_pairs` gains a `category` column (`tool_output`, `assistant_reasoning`, `user_context`, `unknown`). `compression_guidelines` table gains a `category` column with `UNIQUE(version, category)` constraint. When `categorized_guidelines = true`, per-category guideline documents are maintained.

### Config
```toml
[memory.compression]
categorized_guidelines = false
```

### Key Invariants
- Category is classified from compaction summary content before the LLM call — never guessed post-hoc
- `UNIQUE(version, category)` constraint prevents guideline duplication per version+category pair
- Scene creation is idempotent: the same batch of messages always produces the same (or equivalent) scenes

---

## Spreading Activation Recall Timeout
> **Status**: Updated. See also `012-graph-memory/spec.md`.

The spreading activation recall operation in graph memory now has a configurable wall-clock timeout. When the timeout elapses, activation stops and returns whatever results have been accumulated so far.

### Config
```toml
[memory.graph]
recall_timeout_ms = 500   # wall-clock timeout for spreading activation in milliseconds; 0 = no timeout
```

### Key Invariants
- `recall_timeout_ms = 0` disables the timeout (legacy behavior)
- Partial results on timeout are returned as-is — the agent must not treat a timeout as an empty result
- NEVER block the agent turn indefinitely on activation — timeout is mandatory for production deployments

---

## Tier Promotion Locking
>  **Status**: Updated.

SQLite tier promotion (moving messages from working memory to long-term storage) now acquires a `BEGIN IMMEDIATE` transaction instead of the previous `BEGIN DEFERRED`. On `SQLITE_BUSY`, promotion retries up to 3 times with exponential backoff before failing.

### Key Invariants
- `BEGIN IMMEDIATE` prevents write-write conflicts during concurrent promotion attempts
- 3-retry backoff is mandatory — single-attempt failure on busy database is not acceptable for tier promotion
- After 3 failed retries, log a `WARN` and skip the promotion for this turn — never hard-fail the agent turn on a lock contention

---

## Orphan Message Soft-Delete
>  **Status**: Implemented.

Messages with content types that are no longer valid (`tool-pair` and `legacy bracket` formats) are soft-deleted when discovered, rather than causing parse errors or being silently ignored. Soft-delete sets `deleted_at` timestamp; the rows remain in SQLite for audit purposes.

### Affected Message Types
| Type | Condition |
|---|---|
| Tool-pair orphan | `tool_use` message without a corresponding `tool_result` in the same turn |
| Legacy bracket orphan | Message using the deprecated `[tool_call]...[/tool_call]` bracket format |

### Key Invariants
- Orphan detection runs during context assembly, not on write — discovery is lazy
- Soft-delete never removes rows — `deleted_at` is set; `SELECT` queries must filter `WHERE deleted_at IS NULL`
- NEVER parse bracket-format messages as active context after v0.18.2 — they are always orphaned

---

## Multi-Vector Chunking
> **Status**: Implemented. Closes #2551, #2552, #2570, #2571, #2586.

Large messages are split into overlapping chunks before embedding. Each chunk produces an independent Qdrant point. At recall time, scores from all chunks belonging to the same message are aggregated (max-pooling by default).

### Config
```toml
[memory.semantic]
multi_vector_enabled = false
chunk_size = 512        # chars per chunk
chunk_overlap = 64      # overlap between consecutive chunks
```

### Key Invariants
- Multi-vector chunking applies to both write (embed) and recall (score aggregation) paths
- Real-time embed paths (not only batch) must apply chunking when `multi_vector_enabled = true`
- All chunk points share the same `message_id` metadata field for aggregation
- NEVER return duplicate message IDs in recall results when chunks match — deduplicate to the highest score

---

## GAAMA: Episode Nodes
> **Status**: Implemented. Closes #2503, #2508.

`GaamaEpisodeNode` represents a compressed episode entry in the graph memory layer. Episodes group semantically related turns into a single retrievable node with a summary, temporal span, and participant list.

### Storage
`graph_episode_nodes` table (new migration) with columns: `id`, `summary`, `start_ts`, `end_ts`, `participant_ids`, `agent_id`.

### Key Invariants
- Episode creation is a background operation — never block the agent turn
- Episode nodes are graph-only — they are not stored in the main `messages` table
- Episode summary is LLM-generated at episode close time — never constructed from raw message concatenation
- NEVER merge two episode nodes once created; create a new higher-level node instead

---

## BATS: Budget Hints and Utility 5-Way Action Policy
> **Status**: Implemented. Closes #2267, #2477, #2613.

BATS (Budget-Aware Token Strategy) injects a budget hint into each turn's system prompt block. The hint signals the remaining token budget to the agent. A 5-way utility action policy governs how the agent responds to budget signals:

| Action | Trigger |
|--------|---------|
| `expand` | Budget ample — allow verbose tool calls |
| `compress` | Budget moderate — prefer compact tool outputs |
| `defer` | Budget tight — skip non-critical tools |
| `summarize` | Budget critical — compact context immediately |
| `halt` | Budget exhausted — stop tool calls, request compaction |

The utility gate defers the System hint injection (the `[SYSTEM]` block with the policy guidance) until AFTER tool results are processed — prevents the hint from appearing before tool calls complete and causing spurious utility-gate skips.

### Config
```toml
[agent]
bats_enabled = false
bats_budget_fraction = 0.80   # fraction of context window considered "ample"
```

### Key Invariants
- Utility hint injection MUST happen after tool results, not before — prevents [skipped] utility messages appearing mid-tool-loop
- `[skipped]` utility messages from the utility gate must NOT be written to semantic memory (excluded in v0.18.2)
- NEVER halt the agent turn on budget signal alone without attempting compaction first
- 5-way action policy is evaluated per-turn; its output never persists between turns

---

## Focus Compression and Density-Aware Budgets
> **Status**: Implemented. Closes #2553, #2510, #2481, #2604.

### Focus Compression
`FocusCompressor` compresses only the messages within the current "focus window" (most recently accessed or focus-pinned messages) rather than the full context. This preserves high-utility recent context while reducing token footprint.

### Contextual Tool Embeddings
Tool outputs are embedded with additional context (the preceding user message + tool name) to improve semantic relevance scoring. This allows recall to distinguish between identical tool outputs produced in different semantic contexts.

### Density-Aware Budgets
The compression budget is computed from message density (token count per message in the candidate range) rather than a fixed byte cap. Dense message sequences get a larger compression budget; sparse sequences get less.

### Config
```toml
[memory.compression]
focus_compression_enabled = false
contextual_tool_embeddings = false
density_aware_budget = false
density_baseline_tokens = 200    # baseline token count used for budget normalization
```

### Key Invariants
- Focus window messages (focus-pinned or recently accessed) are excluded from Focus Compression targets
- Contextual tool embeddings add overhead — disabled by default; enable only when semantic tool recall is a bottleneck
- Density budget is computed per compaction batch, not once at session start

---

## Persona Memory Layer
> **Status**: Implemented. Closes #2461. Migration 066.

`crates/zeph-memory/src/semantic/persona.rs` — fourth memory tier. User attributes (preferences, domain knowledge, working style, communication style, background) are extracted from conversation history via a cheap LLM provider and injected into context assembly immediately after the system prompt.

Extraction uses a self-referential language heuristic gate to avoid unnecessary LLM calls. Contradictory facts are resolved via `supersedes_id` FK: the extraction LLM classifies extracted facts as NEW or UPDATE and marks older conflicting facts as superseded so they are excluded from context.

### Config
```toml
[memory.persona]
enabled = false
persona_provider = ""          # cheap/fast model; falls back to primary when empty
min_confidence = 0.6           # minimum confidence for facts included in context
min_messages = 3               # minimum user messages before extraction runs
max_messages = 10              # maximum messages per extraction pass
extraction_timeout_secs = 10
context_budget_tokens = 500
```

### Key Invariants
- Persona extraction is fire-and-forget (`tokio::spawn`) — never block the agent turn
- `supersedes_id` FK marks superseded facts so they are excluded from context injection
- Facts with `source_conversation_id` pointing to a deleted conversation are stored with `NULL` provenance — fact is preserved, provenance link is dropped
- NEVER inject superseded facts into context

---

## Multi-Agent Memory Consistency
> **Status**: Implemented. Closes #2478. Migrations 067–068.

### Write Buffer
Session-scoped `WriteBuffer` batches memory writes per turn into a single `BEGIN IMMEDIATE` SQLite transaction, reducing lock contention.

### Advisory Entity Locking
`entity_advisory_locks` table (migration 067) with 120s TTL and `extend_lock()` method prevents duplicate entity resolution when concurrent sessions run.

### Epoch-Based Qdrant Invalidation
`embedding_epoch` column (migration 068) detects stale embeddings via `EmbeddingStore::is_epoch_current()`.

### Key Invariants
- `WriteBuffer` uses `BEGIN IMMEDIATE` — prevents write-write conflicts from concurrent sessions
- Advisory locks have a 120s TTL — never hold indefinitely
- Epoch invalidation is lazy — stale embeddings are detected on read, not eagerly purged

---

## Trajectory-Informed Memory
> **Status**: Implemented. Closes #2498. Migrations 069.

`crates/zeph-memory/src/semantic/trajectory.rs` — after each agent turn containing tool calls, a fast LLM provider extracts procedural (reusable how-to patterns) and episodic (one-off event) entries. Entries are stored per-conversation in `trajectory_memory` / `trajectory_meta` tables. Top-k procedural entries above a confidence threshold are injected into context assembly as "past experience" hints.

Extraction is fire-and-forget (`tokio::spawn`) — no latency added. Per-conversation watermarking via `trajectory_meta(conversation_id PK)` prevents duplicate extraction across concurrent sessions.

CLI: `zeph memory trajectory`. TUI: `/memory trajectory`.

### Config
```toml
[memory.trajectory]
enabled = false
trajectory_provider = ""       # fast/cheap model; falls back to primary when empty
context_budget_tokens = 400
max_messages = 10
extraction_timeout_secs = 10
recall_top_k = 5
min_confidence = 0.6
```

### Key Invariants
- Extraction is always fire-and-forget — never block the agent turn
- Per-conversation watermarking via `trajectory_meta` prevents duplicate extraction
- Only procedural entries are injected into context (not episodic)
- `with_trajectory_config` must be called in all entry points (`runner.rs`, `acp.rs`, `daemon.rs`) — omission silently disables trajectory extraction

---

## Category-Aware Memory
> **Status**: Implemented. Closes #2428. Migration 070.

Nullable `category TEXT` column added to `messages` table with a partial index for filtered recall. `SearchFilter` gains an optional `category` field that adds a Qdrant `FieldCondition` when set. Auto-tagging from active skill/tool context via `save_message_with_category`.

### Config
```toml
[memory.category]
enabled = false
auto_tag = true   # automatically assign category from skill/tool context
```

### Key Invariants
- Category column is nullable — uncategorized messages are never excluded from recall
- `auto_tag` uses skill metadata and tool type at write time — not re-tagged retroactively
- `with_category_config` must be called in all entry points — omission silently disables auto-tagging

---

## TiMem: Temporal-Hierarchical Memory Tree
> **Status**: Implemented. Closes #2262. Migration 071.

`memory_tree` SQLite table stores leaf nodes at level 0 and LLM-merged summaries at higher levels. A background consolidation loop clusters unconsolidated leaf nodes by cosine similarity and merges each cluster into a parent node via LLM summarization. Each cluster merge runs in its own SQLite transaction (prevents `SQLITE_BUSY` contention). Traversal from leaf to root via `traverse_tree_up`.

CLI: `zeph memory tree`. TUI: `/memory tree`.

### Config
```toml
[memory.tree]
enabled = false
consolidation_provider = ""    # fast/cheap model; falls back to primary when empty
sweep_interval_secs = 300
batch_size = 20
similarity_threshold = 0.8
max_level = 3
min_cluster_size = 2
recall_top_k = 5
context_budget_tokens = 400
```

### Key Invariants
- Background consolidation loop runs in a separate tokio task — never blocks agent turns
- Each cluster merge is its own SQLite transaction — prevents `SQLITE_BUSY` under concurrent writes
- `max_level` caps tree depth — never create nodes above this level
- NEVER merge two tree nodes once created; create a new higher-level parent node instead

---

## Key Facts Semantic Dedup
> **Status**: Implemented. Closes #2717.

`store_key_facts` checks for near-duplicate entries before inserting into `zeph_key_facts`. Before each insert, the fact's embedding vector queries the collection for the top-1 nearest neighbour; if the best cosine score is >= `key_facts_dedup_threshold` the fact is silently skipped.

Dedup is fail-open: a search error causes the fact to be stored, not dropped.

### Config
```toml
[memory]
key_facts_dedup_threshold = 0.95   # cosine threshold for considering a fact a duplicate
```

### Key Invariants
- Dedup check runs before every insert — never skip
- A search error means the fact is stored (fail-open) — dedup failure must not lose data
- Policy-decision facts (containing `"blocked"`, `"skipped"`, `"cannot access"`, `"permission denied"`, etc.) are filtered at store time before dedup check — they are never stored

---

## Policy-Decision Fact Filtering
> **Status**: Implemented. Closes #2724.

`is_policy_decision_fact()` performs case-insensitive substring matching. Facts containing transient enforcement language are rejected before embedding and Qdrant insertion. Prevents the agent from believing previously-blocked tool calls are permanently unavailable.

Blocked terms include: `"blocked"`, `"skipped"`, `"cannot access"`, `"permission denied"`.

### Key Invariant
- Filter runs before embedding and dedup — no policy-decision fact ever reaches Qdrant

---

## Time-Based Microcompact
> **Status**: Implemented. Closes #2699.

After an idle gap exceeding `gap_threshold_minutes`, stale low-value tool outputs (`bash`, `shell`, `grep`, `read`, `web_fetch`, etc.) are stripped in-place from the context window and replaced with a `[cleared — stale tool output after Xmin idle]` sentinel. Zero LLM cost — purely in-memory. Wired to `advance_context_lifecycle()`.

A cache-expiry warning is also emitted before the next LLM turn when `microcompact.enabled` and the gap threshold passes: `"Cache expired (~N tokens will be sent uncached on next turn)"`. Uses `providers.cached_prompt_tokens` when non-zero; falls back to a generic message.

### Config
```toml
[memory.microcompact]
enabled = false
gap_threshold_minutes = 60   # minimum idle gap before clearing stale tool outputs
keep_recent = 3               # most recent compactable tool messages to preserve
```

### Key Invariants
- Microcompact is always in-memory — zero LLM cost, zero persistence
- `keep_recent` most-recent compactable tool messages are always preserved
- Cache-expiry warning reuses `microcompact.enabled` and `gap_threshold_minutes` — no separate config
- NEVER microcompact `focus_pinned` messages

---

## autoDream Background Memory Consolidation
> **Status**: Implemented. Closes #2697.

Post-session hook that runs `zeph_memory::run_consolidation_sweep()` in the background when both a session-count gate (`min_sessions`) and a time gate (`min_hours`) pass. Uses a configurable `consolidation_provider`. Bounded by `max_iterations * 30s` timeout. State is in-process only (resets on restart).

### Config
```toml
[memory.autodream]
enabled = false
min_sessions = 3              # minimum sessions between consolidations
min_hours = 24                # minimum hours between consolidations
consolidation_provider = ""   # falls back to primary when empty
max_iterations = 8            # agent loop iterations for consolidation subagent
```

### Key Invariants
- autoDream runs only after the agent loop exits — never during an active session
- State is session-only (`AutoDreamState`) — both gates reset on process restart
- Consolidation is bounded by `max_iterations * 30s` timeout — never runs indefinitely
- NEVER block session startup or shutdown on autoDream consolidation

---

## SleepGate Forgetting Pass
> **Status**: Implemented. Closes #2614.

`SleepGate` is a background forgetting pass that runs outside active agent turns. It applies the Ebbinghaus-inspired forgetting curve to all messages and marks those with retention score below `forget_threshold` as `forgotten` (soft-delete: `forgotten_at` timestamp set).

### Performance-Floor Compression Predictor
Before compacting, a predictor estimates the expected compression ratio based on message density and content type. If the predicted ratio falls below `compression_floor`, compaction is skipped for that batch — preventing wasted LLM calls on incompressible content.

### Config
```toml
[memory.sleep_gate]
enabled = false
forget_threshold = 0.05      # retention score below which messages are soft-forgotten
run_interval_secs = 3600     # how often the forgetting pass runs
compression_floor = 0.20     # minimum expected compression ratio; skip if predicted below
```

### Key Invariants
- `SleepGate` runs only between agent turns — NEVER during an active inference
- Forgotten messages are soft-deleted (`forgotten_at` set) — never hard-deleted
- `SELECT` queries for context assembly must filter `WHERE forgotten_at IS NULL`
- Compression predictor uses heuristics only — no LLM call for prediction
- `compression_floor = 0.0` disables the predictor floor check (always compress)
