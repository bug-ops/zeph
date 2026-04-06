# Graph Memory

Graph memory augments Zeph's existing vector + keyword search with entity-relationship tracking. It stores entities, relationships, and communities extracted from conversations in SQLite, enabling multi-hop reasoning, temporal fact tracking, and cross-session entity linking.

> **Status:** Experimental.

## Why Graph Memory?

Flat vector search finds semantically similar messages but cannot answer relationship questions:

| Question type | Vector search | Graph memory |
|---------------|--------------|--------------|
| "What did we discuss about Qdrant?" | Good | Good |
| "How is project X related to tool Y?" | Poor | Good |
| "What changed since the user switched from vim to neovim?" | Poor | Good |
| "What tools does the user prefer for Rust?" | Partial | Good |

Graph memory tracks **who/what** (entities), **how they relate** (edges), and **when facts change** (bi-temporal timestamps).

## Data Model

### Entities

Named nodes with a type. Each entity has a **canonical name** (normalized, lowercased) used as the unique key, and a **display name** (the most recently seen surface form). Stored in `graph_entities` with a `UNIQUE(canonical_name, entity_type)` constraint.

| Entity type | Examples |
|-------------|----------|
| `person` | User, Alice, Bob |
| `tool` | neovim, Docker, cargo |
| `concept` | async/await, REST API |
| `project` | zeph, my-app |
| `language` | Rust, Python, SQL |
| `file` | main.rs, config.toml |
| `config` | TOML settings, env vars |
| `organization` | Acme Corp, Mozilla |

### Entity Aliases

Multiple surface forms can refer to the same canonical entity. The `graph_entity_aliases` table maps variant names to entity IDs. For example, "Rust", "rust-lang", and "Rust language" can all resolve to the same entity with canonical name "rust".

The entity resolver checks aliases before creating a new entity:

1. Normalize the input name (trim, lowercase, strip control characters, truncate to 512 bytes)
2. Search existing aliases for a match with the same entity type
3. If found, reuse the existing entity and update its display name
4. If not found, create a new entity and register the normalized name as its first alias

This prevents duplicate entities caused by trivial name variations.

### Edges (MAGMA Typed Edges)

Directed relationships between entities. Each edge carries:

- **relation** — verb describing the relationship (`prefers`, `uses`, `works_on`)
- **edge type** — one of five typed categories (see below)
- **fact** — human-readable sentence ("User prefers neovim for Rust development")
- **confidence** — 0.0 to 1.0 score
- **bi-temporal timestamps** — `valid_from`/`valid_until` for fact validity, `created_at`/`expired_at` for ingestion time

#### Edge Types

MAGMA (Multi-graph Attribute-typed Graph Memory Architecture) classifies edges into five semantic types, enabling type-aware traversal and filtering:

| Edge Type | Description | Example |
|-----------|-------------|---------|
| `Causal` | One entity caused or led to another | "Refactoring X caused bug Y" |
| `Temporal` | Time-ordered sequence or succession | "Vim was replaced by neovim" |
| `Semantic` | Meaning-based association | "Rust is related to memory safety" |
| `CoOccurrence` | Entities appeared together in context | "Docker and Kubernetes co-occur" |
| `Hierarchical` | Parent-child or part-whole relationship | "auth.rs belongs to the auth module" |

Edge types are extracted by the LLM during background extraction and stored alongside the relation string. Type-aware queries can filter or weight edges by type during retrieval.

When a fact changes (e.g., user switches from vim to neovim), the old edge is invalidated (`valid_until` and `expired_at` set) and a new edge is created. Both are preserved for temporal queries.

Partial indexes on `(source_entity_id, valid_from) WHERE valid_to IS NOT NULL` and `(target_entity_id, valid_from) WHERE valid_to IS NOT NULL` accelerate temporal range queries (migration 030).

Active edges are deduplicated on `(source_entity_id, target_entity_id, relation)`. When the same relation is re-extracted, the existing row is updated with the higher confidence value instead of creating a duplicate row. This prevents repeated extractions from inflating edge counts over long conversations.

### Communities

Groups of related entities with an LLM-generated summary. Community detection runs periodically via label propagation (Phase 5).

## Background Extraction

After each user message is persisted, Zeph spawns a background extraction task (when `[memory.graph] enabled = true`). The extraction pipeline:

1. Collects the last 4 user messages as conversational context
2. Sends the current message plus context to the configured LLM (`extract_model`, or the agent's primary model when empty)
3. Parses the LLM response into entities and edges, respecting `max_entities_per_message` and `max_edges_per_message` limits
4. Upserts extracted data into SQLite with bi-temporal timestamps

Extraction runs non-blocking via `spawn_graph_extraction` — the agent loop continues without waiting for it to finish. A configurable timeout (`extraction_timeout_secs`, default: 15) prevents slow LLM calls from accumulating.

### Security

Messages flagged with injection patterns are excluded from extraction. When the content sanitizer detects injection markers (`has_injection_flags = true`), `maybe_spawn_graph_extraction` returns early without queuing any work. This prevents untrusted content from poisoning the knowledge graph.

### TUI Status

During extraction, the TUI displays an "Extracting entities..." spinner so the user knows background work is in progress.

## Entity Resolution

By default, entities are deduplicated using exact name matching. When `use_embedding_resolution = true`, Zeph uses cosine similarity search in Qdrant to find semantically equivalent entities before creating new ones.

The resolution logic uses a two-threshold approach:

| Similarity | Action |
|-----------|--------|
| >= `entity_similarity_threshold` (default: 0.85) | Auto-merge with the existing entity |
| >= `entity_ambiguous_threshold` (default: 0.70) | LLM disambiguation — the model decides whether to merge or create |
| Below 0.70 | Create a new entity |

This handles cases where the same concept appears under different names (e.g., "VS Code" and "Visual Studio Code", "k8s" and "Kubernetes"). On any failure (Qdrant unavailable, embedding error), resolution falls back to exact match silently.

Configure in `[memory.graph]`:

```toml
[memory.graph]
use_embedding_resolution = true     # default: false
entity_similarity_threshold = 0.85  # auto-merge threshold
entity_ambiguous_threshold = 0.70   # LLM disambiguation threshold
```

## Retrieval: BFS Traversal

Graph recall uses breadth-first search to find relevant facts:

1. Match query to entities (by name or embedding similarity)
2. Traverse edges up to `max_hops` (default: 2) from matched entities
3. Collect active edges (`valid_until IS NULL`) along the path
4. Score facts using `composite_score = entity_match * (1 / (1 + hop_distance)) * evolved_weight(retrieval_count, confidence)`

The BFS implementation is cycle-safe and uses at most `max_hops + 2` SQLite queries regardless of graph size.

## A-MEM Link Weight Evolution

Edges accumulate a `retrieval_count` — the number of times they were traversed during graph recall. Each traversal increments the counter and the edge's effective weight in scoring is computed as:

```
evolved_weight(count, confidence) = confidence * (1.0 + 0.2 * ln(1.0 + count)).min(1.0)
```

At `count = 0` the weight equals the base confidence. At `count = 1` it is boosted by ~14%; at `count = 10` by ~48%. The boost is capped at `1.0` regardless of count.

This means frequently retrieved edges — facts the agent has found useful many times — gradually rise in composite score and appear earlier in recall results. Edges that are never traversed remain at base confidence.

### Link Weight Decay

A background decay task can periodically reduce `retrieval_count` to prevent indefinite accumulation:

```toml
[memory.graph]
link_weight_decay_lambda = 0.95          # Multiplicative decay per interval, (0.0, 1.0] (default: 0.95)
link_weight_decay_interval_secs = 86400  # Decay interval in seconds (default: 24h)
```

With `decay_lambda = 0.95`, each decay pass multiplies `retrieval_count` by 0.95, slowly reducing the influence of stale traversals. Set `decay_lambda = 1.0` to disable decay entirely.

## SYNAPSE Spreading Activation

SYNAPSE (SYNaptic Activation and Propagation for Semantic Exploration) is an alternative retrieval strategy that replaces BFS with biologically inspired spreading activation over the entity graph. When enabled, it provides richer multi-hop recall with natural decay and lateral inhibition.

### Hybrid Seed Selection

Before spreading activation, SYNAPSE selects seed entities using hybrid ranking that combines FTS5 full-text score with structural importance:

```
hybrid_score = fts_score * (1 - seed_structural_weight) + structural_score * seed_structural_weight
```

`structural_score` is derived from an entity's degree (number of active edges) and edge-type diversity. This prioritizes structurally central entities as seeds even when their name match is weak.

| Field | Default | Description |
|-------|---------|-------------|
| `seed_structural_weight` | `0.4` | Weight of structural score in hybrid ranking (`[0.0, 1.0]`) |
| `seed_community_cap` | `3` | Maximum seed entities per community; `0` = unlimited |

`seed_community_cap` prevents a single dense community from monopolizing all seed slots, encouraging coverage across unrelated parts of the graph.

### How Spreading Works

1. **Seed activation** — matched entities receive activation level 1.0
2. **Propagation** — activation spreads along edges, decaying by `decay_lambda` per hop: `activation(hop) = parent_activation * decay_lambda`
3. **Lateral inhibition** — when an entity's activation exceeds `inhibition_threshold` (default: 0.8), it suppresses activation of neighboring entities. This prevents highly connected hub nodes from dominating results
4. **Threshold gating** — entities with activation below `activation_threshold` (default: 0.1) are excluded from results
5. **Timeout** — the entire activation process is bounded by a 500ms timeout to prevent runaway computation on large graphs

### Edge-Type Filtering

SYNAPSE leverages MAGMA typed edges during propagation. Activation flows preferentially along `Causal` and `Semantic` edges, with reduced flow along `CoOccurrence` edges. This produces more semantically coherent activation patterns compared to untyped BFS.

### Configuration

```toml
[memory.graph.spreading_activation]
enabled = true                      # Replace BFS with spreading activation (default: false)
decay_lambda = 0.85                 # Per-hop decay factor, (0.0, 1.0] (default: 0.85)
max_hops = 3                        # Maximum propagation depth (default: 3)
activation_threshold = 0.1          # Minimum activation to include in results (default: 0.1)
inhibition_threshold = 0.8          # Activation level triggering lateral inhibition (default: 0.8)
max_activated_nodes = 50            # Cap on activated nodes to return (default: 50)
seed_structural_weight = 0.4        # Structural score weight in hybrid seed ranking (default: 0.4)
seed_community_cap = 3              # Max seeds per community; 0 = unlimited (default: 3)
```

| Field | Default | Constraint |
|-------|---------|------------|
| `decay_lambda` | 0.85 | Must be in (0.0, 1.0] |
| `activation_threshold` | 0.1 | Must be < `inhibition_threshold` |
| `inhibition_threshold` | 0.8 | Must be > `activation_threshold` |

When `spreading_activation.enabled = false` (the default), graph recall uses BFS as described above.

### Temporal Queries

Two temporal query methods allow point-in-time fact retrieval:

| Method | Description |
|--------|-------------|
| `edges_at_timestamp(entity_id, timestamp)` | Returns all edges where `valid_from <= timestamp` and (`valid_until IS NULL` OR `valid_until > timestamp`). Covers both active and historically valid edges. |
| `bfs_at_timestamp(start_entity_id, max_hops, timestamp)` | BFS traversal that only follows edges valid at the given timestamp. Returns entities, edges, and depth map. |
| `edge_history(source_entity_id, predicate, relation?, limit)` | All historical versions of edges matching a predicate, ordered `valid_from DESC` (most recent first). LIKE wildcards in the predicate are escaped. |

Timestamps must be SQLite datetime strings: `"YYYY-MM-DD HH:MM:SS"`.

### Temporal Decay Scoring

When `temporal_decay_rate > 0`, a recency boost is applied to graph fact scores:

```
boost = 1 / (1 + age_days * temporal_decay_rate)
final_score = base_score + boost (capped at 2× base)
```

With `temporal_decay_rate = 0.0` (default), scoring is unchanged. The `temporal_decay_rate` field is validated at deserialization: finite values in `[0.0, 10.0]` only; NaN and Inf are rejected.

## Community Detection

Community detection groups related entities into clusters using label propagation. Instead of treating the knowledge graph as a flat collection of facts, communities reveal thematic clusters — for example, a group of entities related to "Rust tooling" or "deployment infrastructure."

### How It Works

Every `community_refresh_interval` messages (default: 100), a background task runs full community detection:

1. Load all entities from SQLite; load active edges in chunks (keyset pagination via `WHERE id > ? LIMIT ?`, chunk size controlled by `lpa_edge_chunk_size`, default: 10,000). Chunked loading reduces peak memory on large graphs compared to loading all edges at once. Set `lpa_edge_chunk_size = 0` to restore the legacy stream-all path.
2. Construct an undirected petgraph graph in memory
3. Run label propagation for up to 50 iterations until convergence: each node adopts the most frequent label among its neighbors, with ties broken by smallest label value
4. Discard groups with fewer than 2 entities
5. Compute a BLAKE3 fingerprint (sorted entity IDs + intra-community edge IDs) for each community. Communities whose membership has not changed since the last detection run skip LLM summarization entirely — a second consecutive run on an unchanged graph triggers zero LLM calls.
6. Generate LLM summaries (2-3 sentences) in parallel for communities whose fingerprint changed, bounded by `community_summary_concurrency` (default: 4) concurrent calls
7. Persist communities to the `graph_communities` SQLite table

### Incremental Assignment

Between full detection runs, newly extracted entities are assigned to existing communities incrementally. When a new entity has edges to entities already in a community, it joins via neighbor majority vote — no full re-detection is triggered. If no neighbors belong to any community, the entity remains unassigned until the next full run.

### Viewing Communities

Use the `/graph communities` TUI command to list detected communities and their summaries (Phase 6).

## Graph Eviction

Graph data grows unboundedly without eviction. Zeph runs three eviction rules during every community refresh cycle to keep the graph manageable.

### Expired Edge Cleanup

Edges invalidated (`valid_to` set) more than `expired_edge_retention_days` days ago are deleted. These are facts superseded by newer information — the active replacement edge is retained.

### Orphan Entity Cleanup

Entities with no active edges and `last_seen_at` older than `expired_edge_retention_days` days are deleted. An entity with no connections that has not been seen recently is stale.

### Entity Count Cap

When `max_entities > 0` and the entity count exceeds the cap, the oldest entities (by `last_seen_at`) with the fewest active edges are deleted first. Set `max_entities = 0` (default) to disable the cap.

### Configuration

Configure eviction in `[memory.graph]`:

- `expired_edge_retention_days` — days to retain expired edges before deletion (default: 90)
- `max_entities` — maximum entities to retain; `0` means unlimited (default: 0)

## Entity Search: FTS5 Full-Text Index

Entity lookup (used by `find_entities_fuzzy`) is backed by an FTS5 virtual table (`graph_entities_fts`) that indexes entity names and summaries. This replaces the earlier `LIKE`-based search with ranked full-text matching.

Key details:

- **Tokenizer:** `unicode61` with prefix matching — handles Unicode names and supports prefix queries (e.g., `rust*`).
- **Ranking:** Uses FTS5 `bm25()` with a 10x weight on the `name` column relative to `summary`, so exact name hits rank above summary-only mentions.
- **Sync:** Insert/update/delete triggers keep the FTS index in sync with `graph_entities` automatically.
- **Migration:** The FTS5 table and triggers are created by migration **023**.

No additional configuration is needed — FTS5 search is used automatically when graph memory is enabled.

## Context Injection

When graph memory contains entities relevant to the current query, Zeph injects a `[knowledge graph]` system message into the context at position 1 (immediately after the base system prompt). Each fact is formatted as:

```text
- Rust uses cargo (confidence: 0.95)
- User prefers neovim (confidence: 0.88)
```

Entity names, relations, and targets are escaped — newlines and angle brackets are stripped — to prevent graph-stored strings from breaking the system prompt structure.

Graph facts receive 3% of the available context budget (carved from the semantic recall allocation, which drops from 8% to 5%). When the budget is zero (unlimited mode) or graph memory is disabled, no budget is allocated and no facts are injected.

## Configuration

Enable graph memory in your `config.toml`:

```toml
[memory.graph]
enabled = true               # Enable graph memory (default: false)
extract_model = ""           # LLM model for extraction; empty = agent's model
max_entities_per_message = 10
max_edges_per_message = 15
max_hops = 2                 # BFS traversal depth (default: 2)
recall_limit = 10            # Max graph facts injected into context
extraction_timeout_secs = 15
entity_similarity_threshold = 0.85
entity_ambiguous_threshold = 0.70
use_embedding_resolution = false  # Enable embedding-based entity dedup
community_refresh_interval = 100  # Messages between community recalculation
community_summary_concurrency = 4 # Parallel LLM calls for community summaries (1 = sequential)
lpa_edge_chunk_size = 10000       # Edges per chunk during community detection (0 = legacy stream-all)
expired_edge_retention_days = 90  # Days to retain expired (superseded) edges
max_entities = 0                  # Entity cap (0 = unlimited)
temporal_decay_rate = 0.0         # Recency boost for graph recall; 0.0 = disabled (default)
                                  # Range: [0.0, 10.0]. Formula: 1/(1 + age_days * rate)
edge_history_limit = 100          # Max versions returned by edge_history() per source+predicate pair

[memory.graph.note_linking]
# enabled = false                 # Enable A-MEM note linking after extraction (default: false)
# similarity_threshold = 0.85     # Min cosine similarity to create a similar_to edge (default: 0.85)
# top_k = 10                      # Max similar entities to link per extracted entity (default: 10)
# timeout_secs = 5                # Linking pass timeout in seconds (default: 5)
# link_weight_decay_lambda = 0.95 # Multiplicative decay factor for retrieval_count, (0.0, 1.0] (default: 0.95)
# link_weight_decay_interval_secs = 86400  # Seconds between decay passes (default: 86400 = 24h)

[memory.graph.spreading_activation]
enabled = false                   # Replace BFS with spreading activation (default: false)
decay_lambda = 0.85               # Per-hop decay factor (default: 0.85)
max_hops = 3                      # Maximum propagation depth (default: 3)
activation_threshold = 0.1        # Minimum activation for inclusion (default: 0.1)
inhibition_threshold = 0.8        # Lateral inhibition threshold (default: 0.8)
max_activated_nodes = 50          # Cap on returned nodes (default: 50)
seed_structural_weight = 0.4      # Structural score weight in hybrid seed ranking (default: 0.4)
seed_community_cap = 3            # Max seeds per community; 0 = unlimited (default: 3)
```

## Schema

Graph memory uses five SQLite tables (created by migrations 021, 023, 024, 027–030, independent of feature flag):

- `graph_entities` — entity nodes with `canonical_name` (unique key) and `name` (display form)
- `graph_entity_aliases` — maps variant names to entity IDs for canonicalization
- `graph_edges` — directed relationships with bi-temporal timestamps (`valid_from`, `valid_until`, `expired_at`)
- `graph_communities` — entity groups with summaries
- `graph_metadata` — persistent key-value counters

Migration 030 adds partial indexes for temporal range queries (see [Temporal Queries](#temporal-queries) above).

A `graph_processed` flag on the existing `messages` table tracks which messages have been processed for entity extraction.

## TUI Commands

All `/graph` commands are available in the interactive session (CLI and TUI):

| Command | Description |
|---------|-------------|
| `/graph` | Show graph statistics: entity, edge, and community counts |
| `/graph entities` | List all known entities with type and last-seen date (capped at 50) |
| `/graph facts <name>` | Show all facts (edges) connected to a named entity. Uses exact case-insensitive match on `name`/`canonical_name` first; falls back to FTS5 prefix search only when no exact match is found. |
| `/graph communities` | List detected communities with names and summaries |
| `/graph backfill [--limit N]` | Extract graph data from existing conversation messages |

Commands that query the database (`/graph entities`, `/graph communities`, `/graph backfill`) emit a
status message before results so you always know what is happening.

## CLI Flag

`--graph-memory` enables graph memory for the session, overriding `memory.graph.enabled` in config:

```sh
zeph --graph-memory
```

> **Note:** The `[memory.graph]` config section must be present in `config.toml` for graph extraction, entity resolution, and BFS recall to activate at startup. Setting `enabled = true` without providing the section leaves graph config at its default state (disabled). Use `zeph --init` to generate the full config structure.

## Configuration Wizard

When running `zeph init`, you will be prompted:

1. **"Enable knowledge graph memory? (experimental)"** — sets `memory.graph.enabled = true`
2. **"LLM model for entity extraction (empty = same as agent)"** — sets `memory.graph.extract_model`
   (leave empty to use the same model as the main agent)

## Backfill

To populate the graph from existing conversations, use `/graph backfill`. This processes all messages
that have not yet been graph-extracted and stores the resulting entities and edges.

```
/graph backfill             # process all unprocessed messages
/graph backfill --limit 100 # process at most 100 messages
```

Backfill runs synchronously in the agent loop and reports progress after each batch of 50 messages.
For large conversation histories, use `--limit` to spread the work across multiple sessions.
LLM costs apply per message processed.

## Implementation Phases

Graph memory is being implemented incrementally:

1. ~~**Schema & Core Types** — migration, types, CRUD store, config~~
2. ~~**Entity & Relation Extraction** — LLM-powered extraction pipeline~~
3. ~~**Graph-Aware Retrieval** — BFS traversal with fuzzy entity matching, composite scoring, and cycle-safe traversal~~
4. ~~**Background Extraction** — non-blocking extraction in agent loop, context injection, budget allocation~~
5. ~~**Community Detection** — label propagation with petgraph, graph eviction~~
6. ~~**TUI & Observability** — `/graph` commands, metrics, init wizard~~

## Belief Revision

Belief revision (Kumiho AGM-inspired) handles the case where a newly extracted fact contradicts an existing one. Without revision, the graph accumulates conflicting beliefs indefinitely.

When `belief_revision.enabled = true`, each new edge is compared against existing active edges for the same source/target entity pair using embedding cosine similarity. If the similarity exceeds `similarity_threshold`, the new fact is considered a contradiction of the existing one:

1. The existing edge is invalidated — `valid_until` and `expired_at` are set, and a `superseded_by` pointer is written linking the old edge to its replacement.
2. The new edge is inserted as the current belief.

Both the old and new edges are preserved for temporal queries. The old edge is visible via `edge_history()` but excluded from active recall.

```toml
[memory.graph.belief_revision]
enabled = false              # Enable contradiction detection and revision (default: false)
similarity_threshold = 0.85  # Cosine similarity threshold for conflict detection (default: 0.85)
```

Belief revision requires an embedding store (`qdrant` or `sqlite` vector backend). On any embedding failure the revision step is skipped and the new edge is inserted normally.

## Note Linking

Note linking (A-MEM) automatically creates `similar_to` edges between semantically similar entities after each extraction pass. This builds a secondary similarity layer on top of the explicitly extracted relation edges, enabling retrieval to traverse conceptual proximity even when no direct relation was stated.

After each extraction completes, every newly extracted entity is compared against the existing entity embedding collection. Entity pairs with cosine similarity above `similarity_threshold` receive a bidirectional `similar_to` edge. The number of links per entity is capped by `top_k` to prevent high-degree hubs.

```toml
[memory.graph.note_linking]
enabled = false              # Enable A-MEM note linking after extraction (default: false)
similarity_threshold = 0.85  # Min cosine similarity to create a similar_to edge (default: 0.85)
top_k = 10                   # Max similar entities to link per extracted entity (default: 10)
timeout_secs = 5             # Linking pass timeout in seconds (default: 5)
```

Note linking requires an embedding store. It runs non-blocking after each extraction and is bounded by `timeout_secs` to prevent slow searches from stalling the pipeline.

## RPE Gate

The RPE (Relevance/Prediction Error) gate is a D-MEM inspired cost-reduction mechanism. Graph extraction via an LLM call is expensive; many conversational turns carry little new factual content. The RPE gate estimates how "surprising" each turn is and skips extraction for low-surprise turns.

Surprise is measured as the divergence between the expected response pattern (rolling average of recent turns) and the actual response. Turns with RPE below `threshold` skip the MAGMA extraction pipeline entirely. A consecutive-skip safety valve (`max_skip_turns`) ensures no turn is silently skipped indefinitely — after `max_skip_turns` consecutive skips, the next turn always triggers extraction regardless of its RPE score.

```toml
[memory.graph.rpe]
enabled = false       # Enable RPE-based extraction gating (default: false)
threshold = 0.3       # RPE below this value skips extraction; range [0.0, 1.0] (default: 0.3)
max_skip_turns = 5    # Max consecutive turns to skip before forcing extraction (default: 5)
```

When `enabled = false` (the default), every turn triggers extraction as before.

## Link Weight Decay

The A-MEM link weight decay mechanism prevents `retrieval_count` from growing without bound. Without decay, edges traversed early in a conversation permanently dominate recall scoring regardless of how stale they become.

A background task runs periodically and multiplies `retrieval_count` by `link_weight_decay_lambda` for all edges that were not traversed since the last decay pass:

```
new_retrieval_count = retrieval_count * link_weight_decay_lambda
```

With the default `lambda = 0.95`, each decay pass reduces unused edge counts by 5%. Over 14 daily passes an edge that was never traversed again decays to roughly half its original count. Set `lambda = 1.0` to disable decay.

These fields live directly under `[memory.graph]`, not under a subsection:

```toml
[memory.graph]
link_weight_decay_lambda = 0.95       # Multiplicative decay per interval, (0.0, 1.0] (default: 0.95)
link_weight_decay_interval_secs = 86400  # Seconds between decay passes (default: 86400 = 24h)
```

Decay interacts with the A-MEM evolved weight formula (see [A-MEM Link Weight Evolution](#a-mem-link-weight-evolution)): decay reduces the effective boost of stale edges while recent retrievals continue to accumulate their count normally.

## Episode Nodes

Every conversation is represented as an **episode node** in the graph. When graph memory is enabled, Zeph calls `ensure_episode(conversation_id)` at the start of each session to create or retrieve an episode record in the `graph_episodes` table. The call is idempotent — repeated calls for the same conversation return the same episode ID.

### Entity Linking

As entities are extracted during a conversation, each entity is linked to the current episode via `link_entity_to_episode(episode_id, entity_id)`, stored in the `graph_episode_entities` join table. This link uses `INSERT OR IGNORE` so re-extracted entities never produce duplicates.

The reverse lookup — all episodes in which a given entity appeared — is available via `episodes_for_entity(entity_id)`. This enables time-aware queries: "which sessions mentioned this entity?", "what entities appeared in the last three sessions?", or "when did we first discuss this concept?"

### Schema

Two tables support episode tracking:

```
graph_episodes (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL UNIQUE,  -- FK → conversations.id
    created_at      DATETIME DEFAULT CURRENT_TIMESTAMP
)

graph_episode_entities (
    episode_id  INTEGER NOT NULL,  -- FK → graph_episodes.id
    entity_id   INTEGER NOT NULL,  -- FK → graph_entities.id
    PRIMARY KEY (episode_id, entity_id)
)
```

### Uses

Episode boundaries are the foundation for temporal reasoning over the knowledge graph:

- **Freshness scoring** — facts from the current episode are more salient than facts from older episodes, complementing the bi-temporal edge timestamps.
- **Session-scoped recall** — retrieve only entities observed in recent sessions without full BFS traversal.
- **Temporal queries** — combine `episodes_for_entity` with `edges_at_timestamp` to reconstruct the agent's knowledge state at any past session boundary.

No configuration is required — episode tracking is always active when `memory.graph.enabled = true`.

## Advanced Tuning

The following fields under `[memory.graph]` control performance and resource usage. They rarely need adjustment in typical deployments.

| Field | Default | Description |
|-------|---------|-------------|
| `community_summary_max_prompt_bytes` | `8192` | Maximum prompt size in bytes fed to the LLM when generating a community summary; truncates long community context to keep costs predictable. |
| `community_summary_concurrency` | `4` | Number of LLM calls issued in parallel during community summarization; lower values reduce concurrent API load at the cost of slower detection runs. |
| `lpa_edge_chunk_size` | `10000` | Edges loaded per chunk during label-propagation community detection; reduces peak memory on large graphs. Set to `0` to load all edges at once (legacy path). |
| `pool_size` | `3` | SQLite connection pool size for the graph tables; kept separate from the main memory pool to prevent starvation when community detection or spreading activation runs concurrently with regular operations. |

```toml
[memory.graph]
community_summary_max_prompt_bytes = 8192
community_summary_concurrency = 4
lpa_edge_chunk_size = 10000
pool_size = 3
```

## See Also

- [Memory & Context](memory.md) — overview of Zeph's memory system
- [Configuration Reference](../reference/configuration.md#memorygraph) — full config reference
- [Feature Flags](../reference/feature-flags.md) — all available feature flags
