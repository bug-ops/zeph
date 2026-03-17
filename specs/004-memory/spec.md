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
