# Spec: Memory System

## Sources

### External
- **A-MEM** (NeurIPS 2025) — agentic write-time memory linking: https://arxiv.org/abs/2502.12110
- **Zep: Temporal Knowledge Graph** (Jan 2025) — `valid_from`/`valid_until` edges, LongMemEval +18.5%: https://arxiv.org/abs/2501.13956
- **TA-Mem** (Mar 2026) — adaptive retrieval dispatch by query type, HeuristicRouter: https://arxiv.org/abs/2603.09297
- **Episodic-to-Semantic Memory Promotion** (Jan 2025): https://arxiv.org/pdf/2501.11739 · https://arxiv.org/abs/2512.13564
- **MAGMA** (Jan 2026) — multi-graph agent memory, 0.70 on LoCoMo: https://arxiv.org/abs/2601.03236
- **Context Engineering in Manus** (Oct 2025) — tool output reference pattern: https://rlancemartin.github.io/2025/10/15/manus/
- **Structured Anchored Summarization** (Factory.ai, 2025) — typed summary schemas: https://factory.ai/news/compressing-context

### Internal
| File | Contents |
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
