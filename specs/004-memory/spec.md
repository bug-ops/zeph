---
aliases:
  - Memory System
  - Memory Pipeline
  - Semantic Memory
tags:
  - sdd
  - spec
  - memory
  - persistence
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec#6. Memory Pipeline Contract]]"
  - "[[002-agent-loop/spec]]"
  - "[[004-6-graph-memory]]"
  - "[[012-graph-memory/spec]]"
  - "[[031-database-abstraction/spec]]"
---

# Spec: Memory System (Parent Index)

> [!info]
> SQLite + Qdrant dual backend, semantic response cache, anchored summarization,
> compaction probe, importance scoring, admission control, and cost-sensitive routing.

## Overview

This is the **parent specification** for the memory subsystem. For detailed information on
specific areas, refer to the child specs below.

---

## Child Specifications

| Spec | Topic | Purpose |
|------|-------|---------|
| [[004-1-architecture]] | Core Pipeline | Conversation storage, message lifecycle, recall architecture |
| [[004-2-compaction]] | Deferred Summaries | Tool pair summarization, context pressure thresholds, compaction probe |
| [[004-3-admission-control]] | A-MAC & Filtering | Five-factor importance scoring, admission gates, noise filtering |
| [[004-4-embeddings]] | Embedding Generation | Batch strategies, backfill, concurrent workers, TUI integration |
| [[004-5-temporal-decay]] | Retention Scoring | Ebbinghaus forgetting curve, access frequency, decay-based eviction |
| [[004-6-graph-memory]] | Graph Memory | Entity graph, BFS recall, MAGMA typed edges, SYNAPSE spreading activation, A-MEM link weights |

---

## System Architecture

```
SemanticMemory (Arc)
├── SqliteStore         — conversation history, message metadata
├── QdrantStore         — vector embeddings for semantic search
├── GraphStore          — entity/edge graph, see [[004-6-graph-memory]]
└── ResponseCache       — deduplicated LLM response cache
```

---

## Key Contracts

### Message Storage
- Every user + assistant turn persisted to SQLite immediately
- Messages are never deleted — only marked with `compacted_at` or summarized
- `MessageMetadata`: `agent_visible`, `user_visible`, `focus_pinned` — all respected
- Conversation identified by `ConversationId`; one per agent session

### Admission Control
- Not all messages admitted to memory (noise filtering via A-MAC)
- Five-factor scoring: recency, relevance, tool-use, entity-density, length
- Threshold-based gate: score < threshold → rejected (returns None)
- Fail-open: admission error → admit message anyway

### Compaction & Eviction
- Soft threshold (~60%) marks tool pairs for summary
- Hard threshold (~90%) applies summaries before LLM call
- Eviction prioritizes low-retention-score messages (Ebbinghaus model)
- Original messages stored in SQLite even after compaction

### Embedding Pipeline
- All admitted messages queued for embedding (async)
- Batched embedding with configurable batch size and timeout
- Backfill at boot recovers unembed messages
- TUI shows queue depth, batch status, backfill progress

### Retention Scoring
- Based on Ebbinghaus forgetting curve: `R(t) = e^(-t / halflife)`
- Boosted by access frequency (messages accessed more often decay slower)
- Scores [0.0, 1.0]: 1.0 fresh+accessed, 0.0 old+never-accessed
- Drives eviction and (optionally) admission decisions

---

## Experience Compression Spectrum

`[memory.compression_spectrum]` (disabled by default, #3305, #3350): introduces
`CompressionLevel` (Episodic / Procedural / Declarative) and a `RetrievalPolicy` that
skips episodic recall when the token budget is below configurable thresholds. A background
`PromotionEngine` scans recent episodic memory and promotes repeated patterns to SKILL.md
entries (off hot path, via `JoinSet`).

`ExperienceStore` records tool outcomes fire-and-forget via `TaskClass::Telemetry`;
evolution sweep runs every N user turns; both gate on `memory.graph.experience.enabled`
with zero overhead when disabled (#3318, #3349).

### Key Invariants

- `PromotionEngine` runs off the hot path — NEVER on the agent turn thread
- `ExperienceStore` wiring must be guarded by `memory.graph.experience.enabled`
- `MemoryError::Promotion` is a distinct error variant in `zeph-memory` (thiserror, no anyhow)

## Sub-Specifications

| Sub-spec | Feature |
|---|---|
| [[004-10-memory-memmachine-retrieval]] | MemMachine retrieval depth, query bias correction, episode preservation |
| [[004-11-memory-hela-mem]] | HeLa-Mem Hebbian edge weights, consolidation, spreading activation |
| [[004-12-memory-reasoning-bank]] | ReasoningBank distilled strategy memory, self-judge pipeline |

## Integration Points

- [[002-agent-loop/spec]] — context assembly calls recall pipeline
- [[001-system-invariants/spec]] — memory pipeline contract
- [[012-graph-memory/spec]] — optional graph-based entity tracking
- [[031-database-abstraction/spec]] — SQLite persistence layer

---

## Sources

### External
- **A-MEM** (NeurIPS 2025) — agentic write-time memory linking: https://arxiv.org/abs/2502.12110
- **Zep: Temporal Knowledge Graph** (Jan 2025) — temporal edges, LongMemEval +18.5%: https://arxiv.org/abs/2501.13956
- **TA-Mem** (Mar 2026) — adaptive retrieval dispatch: https://arxiv.org/abs/2603.09297
- **Episodic-to-Semantic Memory Promotion** (Jan 2025): https://arxiv.org/pdf/2501.11739
- **MAGMA** (Jan 2026) — multi-graph agent memory: https://arxiv.org/abs/2601.03236
- **Context Engineering in Manus** (Oct 2025) — tool output reference pattern: https://rlancemartin.github.io/2025/10/15/manus/
- **Structured Anchored Summarization** (Factory.ai, 2025) — typed schemas: https://factory.ai/news/compressing-context

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/semantic/mod.rs` | `SemanticMemory`, recall pipeline, compaction |
| `crates/zeph-memory/src/graph/mod.rs` | Graph memory integration |
| `crates/zeph-llm/src/provider.rs` | `MessagePart`, `MessageMetadata` definitions |
| `crates/zeph-core/src/agent/mod.rs` | `MemoryState`, deferred summary apply logic |

---

## See Also

- [[MOC-specs]] — Master index of all specifications
- [[001-system-invariants/spec]] — System-wide non-negotiable rules
