---
aliases:
  - HeLa-Mem
  - Hebbian Memory
  - Episodic-Semantic Graph
tags:
  - sdd
  - spec
  - memory
  - graph
  - research
created: 2026-04-24
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-6-graph-memory]]"
  - "[[004-7-memory-apex-magma]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: HeLa-Mem — Hebbian Learning Episodic-Semantic Memory

> Research analysis of HeLa-Mem (arXiv:2604.16839). Resolves GitHub issue
> [#3324](https://github.com/bug-ops/zeph/issues/3324).

## Sources

### External
- **HeLa-Mem: Hebbian Learning and Associative Memory for LLM Agents** (arXiv:2604.16839)
- Benchmark: LoCoMo (long-conversation memory)
- Inspired by: Hebbian learning rule, spreading activation theory (cognitive neuroscience)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/graph/store.rs` | Edge CRUD, petgraph-backed graph |
| `crates/zeph-memory/src/graph/types.rs` | `Edge`, `EdgeType`, weight fields |
| `crates/zeph-memory/src/semantic/graph.rs` | SYNAPSE spreading activation |
| `crates/zeph-memory/src/semantic/mod.rs` | SemanticMemory orchestrator |
| `crates/zeph-skills/src/registry.rs` | Skill storage — target for Hebbian promotion |

---

## 1. Overview

### Problem Statement

Zeph's episodic memory graph stores edges with static weights — there is no mechanism for reinforcing edges that are repeatedly co-activated, nor for automatically promoting densely connected memory clusters to the skills/rules layer. This means frequently accessed knowledge remains in the same retrieval tier as rarely accessed knowledge, and valuable patterns must be manually extracted as skills.

### Key Research Approach

HeLa-Mem introduces three bio-inspired mechanisms:

1. **Association** — when memory nodes are co-activated during retrieval, the edge between them is strengthened (Hebbian rule: "neurons that fire together wire together")
2. **Consolidation** — a periodic pass identifies densely connected clusters (high edge weight × high degree) and promotes them into semantic knowledge (skills or persistent rules)
3. **Spreading Activation** — retrieval propagates from the query anchor node through weighted edges, preferring highly reinforced paths over flat cosine ANN

### Why This Matters for Zeph

- Zeph already uses `petgraph` for the memory graph and has a SYNAPSE spreading activation module — this spec extends both
- Current retrieval is cosine-ANN only; spreading activation would leverage the existing graph structure
- Automatic promotion of hot clusters to skills reduces the dependency on manual `SKILL.md` authoring for recurring patterns

---

## 2. Requirements

### Functional

| ID | Requirement |
|---|---|
| HL-F1 | `Edge` struct gains a `weight: f32` field (default 1.0); stored in SQLite alongside existing temporal fields |
| HL-F2 | On each memory retrieval: increment `weight` for all edges between co-activated node pairs by a configurable `hebbian_lr` (default 0.1) |
| HL-F3 | Periodic consolidation task (configurable interval): identify nodes with `degree × avg_weight > consolidation_threshold`; emit consolidation candidates as structured events |
| HL-F4 | Consolidation candidates are passed to a mid-tier LLM provider for strategy extraction and stored as `PersistentRule` entries (existing type) or new skill drafts |
| HL-F5 | Spreading activation retrieval mode: optional alternative to cosine-ANN, propagates from top-1 ANN match through weighted edges up to `spread_depth` hops |
| HL-F6 | Config section `[memory.hebbian]` controlling all parameters |

### Non-Functional

| ID | Requirement |
|---|---|
| HL-NF1 | Weight increment must be a single `UPDATE` statement per edge pair, batched at end of retrieval — no per-node round-trips |
| HL-NF2 | Consolidation pass runs in background task; must not block the agent turn |
| HL-NF3 | Spreading activation adds ≤ 10ms to retrieval p95 at graph size < 10k nodes |

---

## 3. Design

### 3.1 Config Schema

```toml
[memory.hebbian]
enabled = false                  # opt-in; false until validated
hebbian_lr = 0.1                 # weight increment per co-activation
consolidation_interval_secs = 3600
consolidation_threshold = 5.0    # degree × avg_weight
consolidate_provider = "fast"    # provider name for strategy distillation
spreading_activation = false     # opt-in spreading activation retrieval
spread_depth = 2                 # BFS hops from anchor node
spread_edge_types = []           # edge type filter; empty = all types
step_budget_ms = 8               # per-step circuit breaker for BFS
```

> [!note] Migration
> Config migration step 31b splices `spreading_activation`, `spread_depth`, `spread_edge_types`,
> and `step_budget_ms` into existing `[memory.hebbian]` sections via `--migrate-config`.

### 3.2 Edge Weight Field

Extend `Edge` in `graph/types.rs`:

```rust
pub struct Edge {
    // existing fields ...
    pub weight: f32,  // Hebbian reinforcement weight, default 1.0
}
```

Migration: `ALTER TABLE graph_edges ADD COLUMN weight REAL NOT NULL DEFAULT 1.0`.

### 3.3 Hebbian Update

After retrieval returns a set of activated nodes `{n₁, n₂, ..., nₖ}`:

```sql
UPDATE graph_edges
SET weight = weight + ?
WHERE (source_id IN (?) AND target_id IN (?))
   OR (target_id IN (?) AND source_id IN (?))
```

Batched single statement; `hebbian_lr` as the increment parameter.

### 3.4 Consolidation Pass

Periodic background task:

1. Query: `SELECT node_id, degree, AVG(weight) FROM ... GROUP BY node_id HAVING degree * AVG(weight) > threshold`
2. For each candidate cluster: collect neighboring node summaries → pass to `consolidate_provider` LLM
3. LLM output: structured strategy summary → stored as `PersistentRule` or enqueued as skill draft
4. Mark consolidated nodes with a `consolidated_at` timestamp to avoid re-consolidation within cooldown period

### 3.5 Spreading Activation (HL-F5)

When `spreading_activation = true`:

1. Fetch top-1 node via cosine ANN as anchor (`hela_spreading_recall`)
2. BFS from anchor up to `spread_depth` hops traversing edges in `spread_edge_types` filter
3. Score each visited node: `path_weight × cosine(query, entity)` where `path_weight = Π edge.weight` along the traversal path; negative cosine clamped to 0.0
4. Multi-path convergence: keep maximum `path_weight` when a node is reached by multiple paths
5. Return ranked node set as retrieval result; apply Hebbian increment to top-k kept edges after retrieval

**Isolated anchor fallback**: when the anchor node has no outgoing edges, returns a single synthetic `HelaFact` with `edge_id=0` scored by the real anchor cosine similarity.

**Circuit breaker**: per-step budget (`step_budget_ms`, default 8 ms); emits `WARN` and returns empty when budget is exhausted.

**Dim-mismatch guard**: `OnceLock<String>` prevents repeated Qdrant probes after a dimension mismatch is detected, avoiding continuous retry loops.

**Helper API additions**:
- `VectorStore::get_points` — default trait method returning `Err(Unsupported)`; Qdrant impl via `GetPointsBuilder::new(...).with_vectors(true)`
- `GraphStore::qdrant_point_ids_for_entities` — SQL helper with 490-entity batch chunks
- `Edge::synthetic_anchor(entity_id)` — marker constructor for isolated-anchor fallback
- `EmbeddingStore::get_vectors_from_collection` — batched vector retrieval helper
- `HelaSpreadRuntime` struct on `SemanticMemory` attached via `with_hebbian_spread()`

Falls back to pure ANN when graph has no edges from anchor node.

---

## 4. Key Invariants

- **NEVER** enable Hebbian updates or consolidation without `[memory.hebbian] enabled = true` — both are opt-in
- **NEVER** perform consolidation on the agent turn thread — must be background task only
- Weight increments must be idempotent under retry (use `weight + delta`, not `weight = value`)
- Consolidation must not delete source episodic nodes — promotion is additive

---

## 5. Acceptance Criteria

- [x] `graph_edges` table has `weight` column after migration 077; existing rows default to 1.0 (HL-F1 — implemented #3344)
- [x] Co-activated node pairs show incremented weights after retrieval when `hebbian.enabled = true` (HL-F2 — implemented #3344)
- [x] Consolidation pass runs on background task (HL-F3/F4 — implemented #3345, #3380)
- [x] Spreading activation retrieval (`hela_spreading_recall`) implemented with BFS, multiplicative path weights, and circuit breaker (HL-F5 — implemented #3346)
- [x] `spread_edge_types`, `step_budget_ms` config fields respected; WARN logged for unrecognised edge type strings
- [x] `enabled = false` disables all Hebbian side effects
- [x] Config migration step 31b adds new spreading activation fields to existing configs
- [x] `cargo nextest run -p zeph-memory` passes

---

## 6. Implementation Priority

**P3 — Research-grade, opt-in.** Both Hebbian update and consolidation are gated behind `enabled = false`. The `weight` field migration is the only mandatory schema change. Spreading activation is the highest-risk component and should be the last sub-feature implemented.

---

## 7. Related Issues

- Closes #3324
