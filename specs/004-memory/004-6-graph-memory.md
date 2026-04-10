---
aliases:
  - Graph Memory
  - Entity Graph
  - Knowledge Graph
tags:
  - sdd
  - spec
  - memory
  - graph
created: 2026-04-10
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[012-graph-memory/spec]]"
  - "[[004-1-architecture]]"
  - "[[004-4-embeddings]]"
---

# Spec: Graph Memory

> [!info]
> Entity graph over conversation history: BFS recall, community detection,
> MAGMA typed edges, SYNAPSE spreading activation.
> Full details: [[012-graph-memory/spec]].

## Overview

Graph memory builds a persistent knowledge graph from conversation turns.
After each turn, `GraphExtractor` fires in the background and extracts named entities
and typed relationships from the message content; `EntityResolver` deduplicates
against the existing graph using embedding similarity before writing to SQLite.

Graph memory is optional — enabled by the `graph-memory` feature flag.
When disabled, all graph calls are no-ops and the `SemanticMemory` API remains unchanged.

---

## Architecture

```
SemanticMemory
└── GraphStore (graph-memory feature)
    ├── entities               — canonical entities + aliases
    ├── edges                  — typed, temporal, versioned relationships
    └── communities            — label propagation clusters + LLM summaries
```

**Extraction pipeline** (fire-and-forget background task):
1. `GraphExtractor` → structured LLM call → `(entities, edges)`
2. `EntityResolver` → normalize → alias lookup → embedding dedup → SQLite write

**Recall pipeline** (two modes, called from `graph_recall`):
- **BFS** (`bfs_typed`): hop-limited breadth-first traversal from FTS5-matched seed entities
- **SYNAPSE spreading activation** (`graph_recall_activated`): iterative score propagation with decay and lateral inhibition; wrapped in a 500 ms timeout

---

## Key Concepts

### MAGMA Typed Edges

Edges carry one of four `EdgeType` values partitioning the graph into orthogonal subgraphs:

| Type | Examples |
|------|----------|
| `semantic` | `uses`, `knows`, `prefers`, `depends_on` |
| `temporal` | `preceded_by`, `followed_by`, `happened_during` |
| `causal` | `caused`, `triggered`, `resulted_in` |
| `entity` | `is_a`, `part_of`, `alias_of` |

Dedup key: `(source_entity_id, target_entity_id, relation, edge_type)` — the same entity pair may carry both Semantic and Causal edges for the same relation string.

### Entity Resolution

Thresholds (embedding cosine similarity):

| Score | Action |
|-------|--------|
| ≥ 0.85 | Merge to existing entity |
| 0.70–0.84 | LLM disambiguation call |
| < 0.70 | Create new entity |

`canonical_name` is **immutable after creation**: lowercase + trimmed + control chars stripped. Per-name serialized locking prevents concurrent duplicate creation.

### A-MEM Link Weight Evolution

Each edge tracks `retrieval_count` + `last_retrieved_at`.  
Effective confidence during traversal:

```
effective_confidence = confidence * min(1.0, 1.0 + 0.2 * ln(1 + retrieval_count))
```

Counts decay daily (`new_count = count * exp(-lambda * elapsed_days)`) independently of the eviction cycle.

---

## Config

```toml
[memory.graph]
enabled = true
extract_provider = "fast"           # [[llm.providers]] name for extraction LLM
max_entities_per_message = 10
max_edges_per_message = 20
max_hops = 2
community_detection_interval_secs = 3600
link_weight_decay_lambda = 0.01
link_weight_decay_interval_secs = 86400

[memory.synapse]
enabled = false
seed_structural_weight = 0.3        # blend FTS5 score with structural centrality
seed_community_cap = 3              # max seeds per community cluster

[memory.graph.spreading_activation]
enabled = false
decay_lambda = 0.85
max_hops = 3
activation_threshold = 0.1
inhibition_threshold = 0.8
max_activated_nodes = 50
```

---

## Key Invariants

- Extraction is always fire-and-forget — **never** await extraction before responding to user
- `canonical_name` is immutable once set — never update; only create new canonical
- `(canonical_name, entity_type)` uniqueness enforced by SQLite `UNIQUE` constraint
- BFS uses only active edges: `valid_to IS NULL AND expired_at IS NULL`
- Spreading activation wrapped in 500 ms timeout — **never** block recall pipeline
- `EdgeType` `FromStr` is case-sensitive — only lowercase accepted (`"semantic"`, not `"Semantic"`)
- `bfs_typed([])` = all edge types (backward-compatible empty-slice default)
- `classify_graph_subgraph` is a pure function — **never** calls LLM or DB
- Community fingerprint (BLAKE3) must be recomputed on any membership or edge change
- A-MEM boost cap at 1.0 — `min(1.0, ...)` is mandatory; decay runs independently of GC

---

## TUI Commands

```
/graph entities [query]   — search entities
/graph edges <entity>     — show entity relations
/graph communities        — list communities
/graph stats              — entity/edge/community counts
/graph clear              — delete all graph data
```

---

## Integration Points

- [[004-1-architecture]] — `GraphStore` is the fourth component inside `SemanticMemory`
- [[004-4-embeddings]] — embedding pipeline generates vectors used by `EntityResolver`
- [[004-5-temporal-decay]] — temporal decay formula reused in `GraphFact::score_with_decay` and SYNAPSE `recency_weight`
- [[012-graph-memory/spec]] — complete reference: data model, full algorithm listings, all invariants with codes (SA-INV-01..10)

---

## See Also

- [[004-memory/spec]] — parent: all memory subsystems
- [[012-graph-memory/spec]] — full graph memory specification
- [[004-1-architecture]] — core dual-backend pipeline
