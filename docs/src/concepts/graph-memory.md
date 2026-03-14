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

### Edges

Directed relationships between entities. Each edge carries:

- **relation** — verb describing the relationship (`prefers`, `uses`, `works_on`)
- **fact** — human-readable sentence ("User prefers neovim for Rust development")
- **confidence** — 0.0 to 1.0 score
- **bi-temporal timestamps** — `valid_from`/`valid_to` for fact validity, `created_at`/`expired_at` for ingestion time

When a fact changes (e.g., user switches from vim to neovim), the old edge is invalidated (`valid_to` and `expired_at` set) and a new edge is created. Both are preserved for temporal queries.

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
3. Collect active edges (`valid_to IS NULL`) along the path
4. Score facts using `composite_score = entity_match * (1 / (1 + hop_distance)) * confidence`

The BFS implementation is cycle-safe and uses at most `max_hops + 2` SQLite queries regardless of graph size.

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
```

## Schema

Graph memory uses five SQLite tables (created by migrations 021, 023, and 024, independent of feature flag):

- `graph_entities` — entity nodes with `canonical_name` (unique key) and `name` (display form)
- `graph_entity_aliases` — maps variant names to entity IDs for canonicalization
- `graph_edges` — directed relationships with bi-temporal timestamps
- `graph_communities` — entity groups with summaries
- `graph_metadata` — persistent key-value counters

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

## See Also

- [Memory & Context](memory.md) — overview of Zeph's memory system
- [Configuration Reference](../reference/configuration.md#memorygraph) — full config reference
- [Feature Flags](../reference/feature-flags.md) — all available feature flags
