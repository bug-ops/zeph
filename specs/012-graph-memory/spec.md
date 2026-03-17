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
