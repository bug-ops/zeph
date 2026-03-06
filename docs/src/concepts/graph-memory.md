# Graph Memory

Graph memory augments Zeph's existing vector + keyword search with entity-relationship tracking. It stores entities, relationships, and communities extracted from conversations in SQLite, enabling multi-hop reasoning, temporal fact tracking, and cross-session entity linking.

> **Status:** Experimental. Requires the `graph-memory` feature flag.

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

### Communities

Groups of related entities with an LLM-generated summary. Community detection runs periodically via label propagation (Phase 5).

## Retrieval: BFS Traversal

Graph recall uses breadth-first search to find relevant facts:

1. Match query to entities (by name or embedding similarity)
2. Traverse edges up to `max_hops` (default: 2) from matched entities
3. Collect active edges (`valid_to IS NULL`) along the path
4. Score facts using `composite_score = entity_match * (1 / (1 + hop_distance)) * confidence`

The BFS implementation is cycle-safe and uses at most `max_hops + 2` SQLite queries regardless of graph size.

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
use_embedding_resolution = false
community_refresh_interval = 100  # Messages between community recalculation
```

The `graph-memory` feature flag must be enabled at compile time. When using pre-built binaries compiled with `--features full`, it is already included.

> **Note:** The `[memory.graph]` config section is always parsed regardless of the feature flag. If `enabled = true` but the feature is not compiled in, graph memory is silently skipped.

## Feature Flag

```bash
# Build with graph memory
cargo build --features graph-memory

# Build with all features (includes graph-memory)
cargo build --features full
```

## Schema

Graph memory uses five SQLite tables (created by migrations 021 and 023, independent of feature flag):

- `graph_entities` — entity nodes with `canonical_name` (unique key) and `name` (display form)
- `graph_entity_aliases` — maps variant names to entity IDs for canonicalization
- `graph_edges` — directed relationships with bi-temporal timestamps
- `graph_communities` — entity groups with summaries
- `graph_metadata` — persistent key-value counters

A `graph_processed` flag on the existing `messages` table tracks which messages have been processed for entity extraction.

## Implementation Phases

Graph memory is being implemented incrementally:

1. ~~**Schema & Core Types** — migration, types, CRUD store, config~~
2. ~~**Entity & Relation Extraction** — LLM-powered extraction pipeline~~
3. ~~**Graph-Aware Retrieval** — BFS traversal with fuzzy entity matching, composite scoring, and cycle-safe traversal~~
4. **Background Extraction** — non-blocking extraction in agent loop
5. **Community Detection** — label propagation with petgraph
6. **TUI & Observability** — `/graph` commands, metrics, init wizard

## See Also

- [Memory & Context](memory.md) — overview of Zeph's memory system
- [Configuration Reference](../reference/configuration.md#memorygraph) — full config reference
- [Feature Flags](../reference/feature-flags.md) — all available feature flags
