# Spec: Graph Memory

## Sources

### External
- **Zep: Temporal Knowledge Graph Architecture** (Jan 2025) — `valid_from`/`valid_until` edges, temporal BFS, LongMemEval +18.5%: https://arxiv.org/abs/2501.13956
- **Graphiti** (Zep, 2025) — reference architecture for temporal KG storage: https://github.com/getzep/graphiti
- **MAGMA** (Jan 2026) — multi-graph agent memory, dual-stream write, 0.70 on LoCoMo: https://arxiv.org/abs/2601.03236
- **Temporal Versioning on KG Edges** (Apr 2025): https://arxiv.org/abs/2504.19413

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/graph/mod.rs` | Public API, integration |
| `crates/zeph-memory/src/graph/types.rs` | `Entity`, `Edge`, `Community`, `GraphFact` |
| `crates/zeph-memory/src/graph/store.rs` | SQLite schema, CRUD operations |
| `crates/zeph-memory/src/graph/extractor.rs` | `GraphExtractor`, LLM-powered entity extraction |
| `crates/zeph-memory/src/graph/resolver.rs` | `EntityResolver`, embedding dedup, per-name locks |
| `crates/zeph-memory/src/graph/retrieval.rs` | `graph_recall`, BFS traversal, scoring |
| `crates/zeph-memory/src/graph/community.rs` | Label propagation, BLAKE3 fingerprint, LLM summaries |

---

`crates/zeph-memory/src/graph/` (feature: `graph-memory`) — entity graph over conversation history.

## Data Model

```
SQLite tables:
├── entities               — (id, name, canonical_name, entity_type, summary,
│                             first_seen_at, last_seen_at, qdrant_point_id)
├── graph_entity_aliases   — surface aliases, foreign key to canonical entity
├── edges                  — (id, source_entity_id, target_entity_id, relation, fact,
│                             confidence: f32, valid_from, valid_to, expired_at,
│                             episode_id, qdrant_point_id)
├── communities            — (id, name, summary, entity_ids[], fingerprint, created_at, updated_at)
└── metadata               — (key, value) — graph-level config and stats
```

- `canonical_name`: normalized (lowercase, trimmed, control chars stripped) — **immutable after creation**
- `name`: preserves original casing for display
- `(canonical_name, entity_type)`: unique constraint — dedup key, cannot change
- `aliases`: each alias belongs to exactly one entity (foreign key enforced)
- `communities.fingerprint`: BLAKE3 over sorted entity+edge IDs — detects membership changes

## Extraction Pipeline

Runs **fire-and-forget** in a background task after each turn:

1. `GraphExtractor` calls LLM with structured output JSON
   - System prompt constraints: conversational text only, no code/config/raw data
   - Min 3-char entity names; 12 recognized entity types
   - Truncated to configurable max entities/edges per message (default 10/20)
   - Parse failures degrade gracefully to `Ok(None)` — never panic
2. `EntityResolver` resolves each extracted entity:
   a. Normalize name: lowercase, trim, strip control + bidi chars, truncate 512 bytes
   b. Exact alias lookup → canonical name lookup
   c. Embedding similarity (if Qdrant available):
      - cosine ≥ 0.85 → merge (same entity)
      - cosine 0.70–0.84 → LLM disambiguation call
      - cosine < 0.70 → create new entity
   d. Per-name serialized locking prevents concurrent duplicate creation
   e. On embed/LLM errors: create new entity (graceful degradation)
3. New entities inserted; aliases merged to canonical
4. New edges inserted with `confidence: f32`, `episode_id` (MessageId link)

The background extraction must **not block the agent loop**.

## Graph Recall

`graph_recall(query)` runs as part of the memory recall pipeline (3rd source):

1. Split query into words, fuzzy-search entities via FTS5 prefix matching (512-char cap)
2. BFS traversal from matched entities up to `max_hops` depth
   - **Temporal BFS**: if `at_timestamp` provided, only active edges used (`valid_to IS NULL AND expired_at IS NULL`)
   - Hop distance = min-distance in undirected graph from seed
3. Build `GraphFact` structs with scoring:
   - `composite_score = entity_match_score × (1 / (1 + hop_distance)) × confidence`
   - Temporal decay (optional): `score_with_decay(rate, now_secs)` blends recency boost additively (capped at 2× base)
4. Deduplicate by `(entity_name, relation, target_name)` — keep highest score
5. Sort by `score_with_decay()` descending, limit results
6. Injected as `MessagePart::CrossSession` parts

## Community Detection

- Label propagation on entity graph (petgraph)
- Runs periodically (not on every turn) — configurable interval
- Communities stored in `communities` table with centroid embeddings
- Used for: finding related entity clusters without full BFS traversal

## Eviction Policy

- Edges with weight below threshold are pruned periodically
- Entities with no edges and low access frequency are archived (not deleted)
- `canonical_id` links are never broken — only merged forward

## `/graph` TUI Commands

```
/graph entities [query]   — search entities
/graph edges <entity>     — show entity relations
/graph communities        — list communities
/graph stats              — entity/edge/community counts
/graph clear              — delete all graph data
```

## Key Invariants

- Extraction is always fire-and-forget — never await extraction before responding to user
- `canonical_name` is immutable once set — never update it; only create new canonical
- `(canonical_name, entity_type)` uniqueness is enforced by SQLite UNIQUE constraint
- Per-name serialized locking prevents concurrent duplicate entity creation
- BFS uses only active edges: `valid_to IS NULL AND expired_at IS NULL`
- Hop distance = min-distance in undirected graph (not directed path length)
- `EntityResolver` checks embedding similarity before LLM disambiguation (LLM is expensive)
- FTS5 prefix search is mandatory — pure embedding search is not sufficient fallback
- Community fingerprint (BLAKE3) must be recomputed when membership or edges change

---

## MAGMA: Multi-Graph Memory with Typed Edges

PR #2077. `crates/zeph-memory/src/graph/types.rs`, `store.rs`, `retrieval.rs`, `extractor.rs`.

### Overview

MAGMA (from arXiv 2601.03236) augments the entity graph with typed edges, partitioning relationships into four orthogonal semantic subgraphs. Each entity pair may now have multiple edges of different types (previously deduplicated by `(source, target, relation)` alone).

### `EdgeType` Enum

```
Semantic  — conceptual links (uses, knows, prefers, depends_on, works_on)
Temporal  — time-ordered events (preceded_by, followed_by, happened_during)
Causal    — cause-effect chains (caused, triggered, resulted_in, led_to)
Entity    — structural/identity (is_a, part_of, instance_of, alias_of)
```

Default: `Semantic`. String representation: lowercase (`"semantic"`, `"temporal"`, `"causal"`, `"entity"`). `FromStr` is case-sensitive — only lowercase accepted. Serde uses `snake_case`.

### Deduplication Key Change

Before MAGMA: `(source_entity_id, target_entity_id, relation)` — unique per direction+relation.
After MAGMA: `(source_entity_id, target_entity_id, relation, edge_type)` — same entity pair may carry both `Semantic` and `Causal` edges with the same relation string.

DB migration 041: adds `edge_type TEXT NOT NULL DEFAULT 'semantic' CHECK(...)`, drops old `uq_graph_edges_active` index, creates new uniqueness constraint and two performance indexes.

### Scoped Retrieval (`bfs_typed`)

`bfs_typed(query, edge_types)` traverses only the specified edge subgraph. Empty `edge_types` = all types (backward-compatible with pre-MAGMA code). `graph_recall()` and `recall_graph()` accept `edge_types: &[EdgeType]`.

`classify_graph_subgraph(query)` — pure heuristic in `router.rs` — maps query keywords to relevant `EdgeType` sets. Context assembly calls this per query before invoking `bfs_typed`. Shared marker constants prevent drift between `classify_graph_subgraph` and `HeuristicRouter`.

### LLM Extraction Update

`ExtractedEdge.edge_type` field added with `#[serde(default)]` for backward compatibility. Extraction prompt updated with `edge_type` classification instructions. LLM must classify each extracted edge as `semantic`, `temporal`, `causal`, or `entity`.

Known issue (#2079): LLM sometimes returns `"technology"` as an `EntityType`, which is not in the enum, causing fallback to `Concept`.

### Key Invariants

- `EdgeType` `FromStr` is case-sensitive — `"Semantic"` is an error; only lowercase accepted
- Dedup key now includes `edge_type` — same `(source, target, relation)` may coexist as both Semantic and Causal
- `bfs_typed([])` = all types — empty slice is the backward-compatible default
- `classify_graph_subgraph` is a pure function with no I/O — it must never call LLM or DB
- `ExtractedEdge.edge_type` default is `Semantic` — existing YAML/JSON handoffs remain valid
- NEVER persist `EdgeType` with mixed case — always store lowercase string

---

## SYNAPSE: Spreading Activation Retrieval

PR #2080. `crates/zeph-memory/src/graph/activation.rs`, `semantic/graph.rs`.

### Overview

SYNAPSE (from arXiv 2601.02744) implements spreading activation over the entity graph as an alternative retrieval mode to BFS. Activation propagates iteratively from seed entities, decaying per hop, with lateral inhibition preventing runaway scores in dense clusters.

### Algorithm

```
Phase 1: Seed initialization
  - Seeds come from fuzzy entity search on query (same as BFS)
  - Seeds with match_score < activation_threshold are skipped

Phase 2: Iterative propagation (for hop in 0..max_hops):
  - Active nodes (score >= activation_threshold) propagate to neighbors
  - Spread formula: spread = node_score × decay_lambda × edge.confidence × recency_weight
  - recency_weight = 1 / (1 + age_days × temporal_decay_rate)  [SA-INV-05]
  - Lateral inhibition: skip neighbor if already at inhibition_threshold in current OR next maps
  - Clamped sum: entry.score = min(1.0, existing + spread_value)  [multi-path convergence]
  - Per-hop pruning: if |activation| > max_activated_nodes → keep top-N by score  [SA-INV-04]

Phase 3: Collect nodes above activation_threshold, sorted descending
```

### Parameters (`SpreadingActivationConfig`)

| Parameter | Default | Description |
|---|---|---|
| `decay_lambda` | 0.85 | Exponential decay per hop (must be > 0.0) |
| `max_hops` | 3 | Maximum propagation depth (must be >= 1) |
| `activation_threshold` | 0.1 | Minimum score to remain active |
| `inhibition_threshold` | 0.8 | Score above which a node stops receiving activation |
| `max_activated_nodes` | 50 | Hard cap enforced per hop |
| `temporal_decay_rate` | (from GraphConfig) | Reuses `GraphConfig.temporal_decay_rate` [SA-INV-05] |

Config validation: `max_hops >= 1`, `decay_lambda > 0.0`, `activation_threshold < inhibition_threshold`.

### Integration

`graph_recall_activated(query, store, params, edge_types)` wraps spreading activation with a 500ms `tokio::time::timeout`. Timeout = `Ok([])`, non-fatal. `recall_graph_activated()` on `SemanticMemory` wraps this for the recall pipeline.

MAGMA integration: `edge_types` parameter filters the subgraph traversed — mirrors `bfs_typed` behavior [SA-INV-08].

`edges_for_entities()` batched query on `GraphStore`: chunks entity IDs at 490 to stay within SQLite bind limit (999 slots shared by source + target).

### `/status` Output

Reports active recall mode: `spreading activation (lambda=0.85, hops=3)` or `BFS`.

### Config

```toml
[memory.graph.spreading_activation]
enabled = false
decay_lambda = 0.85
max_hops = 3
activation_threshold = 0.1
inhibition_threshold = 0.8
max_activated_nodes = 50
```

### Key Invariants (SA-INV-01..09)

- SA-INV-01: Seeds bypass `activation_threshold` — they are anchors; sub-threshold seeds are skipped with debug log
- SA-INV-02: Decay per hop: `spread = score × decay_lambda × confidence × recency`
- SA-INV-03: Lateral inhibition checks BOTH the current activation map and the current hop's `next_activation` map
- SA-INV-04: Per-hop pruning enforces `max_activated_nodes` — never let the map grow unbounded
- SA-INV-05: `temporal_decay_rate` reuses the same parameter and formula as `GraphFact::score_with_decay`
- SA-INV-06: Clamped sum (`min(1.0, existing + spread)`) preserves multi-path convergence signal
- SA-INV-07: 500ms timeout is mandatory — never block the recall pipeline on spreading activation
- SA-INV-08: `edge_types` filter in spreading activation mirrors `bfs_typed` — same MAGMA subgraph semantics
- SA-INV-09: `edges_for_entities` chunks at 490 IDs — never exceed SQLite 999-slot bind limit
- NEVER await spreading activation without the 500ms timeout
- NEVER let activation exceed 1.0 (clamped sum invariant)
