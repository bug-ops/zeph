# zeph-memory

[![Crates.io](https://img.shields.io/crates/v/zeph-memory)](https://crates.io/crates/zeph-memory)
[![docs.rs](https://img.shields.io/docsrs/zeph-memory)](https://docs.rs/zeph-memory)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Semantic memory with SQLite and Qdrant for Zeph agent.

## Overview

Provides durable conversation storage via SQLite and semantic retrieval through Qdrant vector search (or embedded SQLite vector backend). The `SemanticMemory` orchestrator combines both backends, enabling the agent to recall relevant context from past conversations using embedding similarity.

Recall quality is enhanced by MMR (Maximal Marginal Relevance) re-ranking for result diversity, temporal decay scoring for recency bias, and write-time importance scoring for content-aware ranking. All are configurable via `SemanticConfig`.

Query-aware memory routing (`MemoryRouter` trait, `HeuristicRouter` default) classifies each query as Keyword (SQLite FTS5), Semantic (Qdrant), or Hybrid and dispatches accordingly. Configure via `[memory.routing]`.

Includes a document ingestion subsystem for loading, chunking, and storing user documents (text, Markdown, PDF) into Qdrant for RAG workflows.

**SYNAPSE spreading activation** enables multi-hop graph retrieval: seed entities are activated, then energy propagates through the entity graph with hop-by-hop decay (configurable lambda), lateral inhibition, and edge-type filtering. Configure via `[memory.graph.spreading_activation]`.

**MAGMA multi-graph memory** provides typed edges (`EdgeType` enum: uses, related_to, part_of, depends_on, created_by, authored, manages, contains) for fine-grained relationship tracking and edge-type-aware traversal.

**Structured anchored summarization** preserves factual anchors (entities, relationships, key decisions) during compaction, producing summaries that maintain cross-session recall fidelity.

**Compaction probe validation** verifies compaction quality by generating probe questions from pre-compaction content and scoring the post-compaction text against them, detecting information loss before it becomes permanent.

## Key modules

| Module | Description |
|--------|-------------|
| `sqlite` | SQLite storage for conversations, messages, and user corrections (`zeph_corrections` table, migration 018 adds `outcome_detail` column); visibility-aware queries (`load_history_filtered` via CTE, `messages_by_ids`, `keyword_search`); durable compaction via `replace_conversation()`; composite covering index `(conversation_id, id)` on messages for efficient history reads |
| `sqlite::history` | Input history persistence for CLI channel |
| `sqlite::acp_sessions` | ACP session and event persistence for session resume, lifecycle tracking, and per-session conversation isolation (migration 026 adds `conversation_id` column) |
| `qdrant` | Qdrant client for vector upsert and search |
| `qdrant_ops` | `QdrantOps` — high-level Qdrant operations |
| `semantic` | `SemanticMemory` — orchestrates SQLite + Qdrant |
| `document` | Document loading, splitting, and ingestion pipeline |
| `document::loader` | `TextLoader` (.txt/.md), `PdfLoader` (feature-gated: `pdf`) |
| `document::splitter` | `TextSplitter` with configurable chunking |
| `document::pipeline` | `IngestionPipeline` — load, split, embed, store via Qdrant |
| `vector_store` | `VectorStore` trait and `VectorPoint` types |
| `sqlite_vector` | `SqliteVectorStore` — embedded SQLite-backed vector search as zero-dependency Qdrant alternative |
| `snapshot` | `MemorySnapshot`, `export_snapshot()`, `import_snapshot()` — portable memory export/import |
| `response_cache` | `ResponseCache` — SQLite-backed LLM response cache with blake3 key hashing and TTL expiry |
| `semantic::importance` | `compute_importance` — write-time importance scoring for messages; scores are blended into recall ranking when `importance_enabled = true` |
| `embedding_store` | `EmbeddingStore` — high-level embedding CRUD |
| `embeddable` | `Embeddable` trait and `EmbeddingRegistry<T>` — generic Qdrant sync/search for any embeddable type |
| `types` | `ConversationId`, `MessageId`, shared types |
| `token_counter` | `TokenCounter` — tiktoken-based (cl100k_base) token counting with DashMap cache (10k cap), OpenAI tool schema formula, 64KB input guard with chars/4 fallback |
| `routing` | `MemoryRouter` trait and `HeuristicRouter` — query-aware routing to Keyword, Semantic, or Hybrid backends |
| `sqlite::overflow` | `tool_overflow` SQLite table (migration 031) — stores large tool outputs keyed by UUID; `SqliteStore::save_overflow` / `SqliteStore::cleanup_overflow` replace the old filesystem backend; `ON DELETE CASCADE` removes overflow rows when the parent conversation is deleted |
| `sqlite::graph_store` | `RawGraphStore` trait and `SqliteGraphStore` — raw JSON-blob persistence for task orchestration graphs (save/load/list/delete); `GraphSummary` metadata type; used by `zeph-core::orchestration::GraphPersistence` for typed serialization |
| `graph` | `GraphStore`, `Entity`, `EntityAlias`, `Edge`, `Community`, `GraphFact`, `EntityType` — knowledge graph with BFS traversal, entity canonicalization, community detection via label propagation, and graph eviction |
| `graph::activation` | `SpreadingActivation` — SYNAPSE spreading activation engine: hop-by-hop energy decay (lambda), edge-type filtering, lateral inhibition, configurable timeout; `ActivatedNode`, `ActivatedFact`, `SpreadingActivationParams` |
| `graph::extractor` | `GraphExtractor` — LLM-powered entity/relation extraction via structured output; `EntityResolver` for dedup and supersession |
| `graph::retrieval` | `graph_recall` — query-time graph retrieval: fuzzy entity matching (including aliases), BFS from seed entities, composite scoring, canonical-name deduplication; spreading activation path via `SpreadingActivation` when enabled |
| `anchored_summary` | `AnchoredSummary` — structured summarization that preserves factual anchors (entities, relationships, decisions) during compaction |
| `compaction_probe` | `CompactionProbeConfig`, `validate_compaction` — post-compaction quality validation via probe question generation and answer scoring |
| `sqlite::experiments` | `ExperimentResultRow`, `NewExperimentResult`, `SessionSummaryRow` — SQLite persistence for experiment results and session summaries (feature-gated: `experiments`) |
| `error` | `MemoryError` — unified error type |

**Re-exports:** `MemoryError`, `QdrantOps`, `ConversationId`, `MessageId`, `Document`, `DocumentLoader`, `TextLoader`, `TextSplitter`, `IngestionPipeline`, `Chunk`, `SplitterConfig`, `DocumentError`, `DocumentMetadata`, `PdfLoader` (behind `pdf` feature), `Embeddable`, `EmbeddingRegistry`, `ResponseCache`, `MemorySnapshot`, `TokenCounter`, `UserCorrection`, `FeedbackDetector`, `AnchoredSummary`, `CompactionProbeConfig`, `validate_compaction`

## Document RAG

`IngestionPipeline` loads, chunks, embeds, and stores documents into the `zeph_documents` Qdrant collection. When `memory.documents.rag_enabled = true`, the agent automatically queries this collection on every turn and prepends the top-K most relevant chunks to the context window.

```bash
zeph ingest ./docs/           # ingest all .txt, .md, .pdf files recursively
zeph ingest README.md --chunk-size 256 --collection my_docs
```

Configure via `[memory.documents]` in `config.toml`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `collection` | string | `"zeph_documents"` | Qdrant collection name for document storage |
| `chunk_size` | usize | `512` | Target token count per chunk |
| `chunk_overlap` | usize | `64` | Overlap between consecutive chunks |
| `top_k` | usize | `3` | Max chunks injected into context per turn |
| `rag_enabled` | bool | `false` | Enable automatic RAG context injection |

> [!NOTE]
> RAG injection is a no-op when the `zeph_documents` collection is empty. Documents must be ingested with `zeph ingest` before retrieval has any effect.

## Snapshot export/import

Memory snapshots allow exporting all conversations and messages to a portable JSON file and importing them back into another instance.

```bash
zeph memory export backup.json
zeph memory import backup.json
```

## Response cache

`ResponseCache` deduplicates LLM calls by caching responses in SQLite. Cache keys are computed via blake3 hashing of the prompt content. Entries expire after a configurable TTL (default: 1 hour). A background task periodically removes expired entries; the interval is controlled by `response_cache_cleanup_interval_secs`.

| Config field | Type | Default | Env override |
|-------------|------|---------|--------------|
| `response_cache_enabled` | bool | `false` | `ZEPH_LLM_RESPONSE_CACHE_ENABLED` |
| `response_cache_ttl_secs` | u64 | `3600` | `ZEPH_LLM_RESPONSE_CACHE_TTL_SECS` |
| `response_cache_cleanup_interval_secs` | u64 | `3600` | — |
| `sqlite_pool_size` | u32 | `5` | — |

## Ranking options

| Option | Config field | Default | Description |
|--------|-------------|---------|-------------|
| MMR re-ranking | `semantic.mmr_enabled` | `false` | Post-retrieval diversity via Maximal Marginal Relevance |
| MMR lambda | `semantic.mmr_lambda` | `0.7` | Balance between relevance (1.0) and diversity (0.0) |
| Temporal decay | `semantic.temporal_decay_enabled` | `false` | Time-based score attenuation favoring recent memories |
| Decay half-life | `semantic.temporal_decay_half_life_days` | `30` | Days until a memory's score drops to 50% |

## User corrections and cross-session personalization

`FeedbackDetector` analyzes each user message for implicit correction signals ("actually", "that's wrong", "no, I meant") and extracts a `UserCorrection` when confidence meets `correction_confidence_threshold`. Corrections are stored in both the `zeph_corrections` SQLite table and the `zeph_corrections` Qdrant collection.

At context-build time, the top-K most similar corrections are retrieved by embedding and injected into the agent context, enabling cross-session personalization without explicit user re-stating preferences.

| Config field | Type | Default | Description |
|---|---|---|---|
| `correction_detection` | bool | `true` | Enable implicit correction detection |
| `correction_confidence_threshold` | f64 | `0.7` | Minimum detector confidence to store a correction |
| `correction_recall_limit` | usize | `5` | Max corrections injected per context-build turn |
| `correction_min_similarity` | f64 | `0.75` | Minimum vector similarity for correction recall |

> [!NOTE]
> Corrections are stored in the `zeph_corrections` Qdrant collection. If you use the `sqlite` vector backend, corrections are stored in the `zeph_corrections` SQLite virtual table instead.

## ACP session storage

`SqliteStore` provides persistence for ACP session lifecycle, event replay, and per-session conversation isolation:

- `create_acp_session_with_conversation(session_id, conversation_id)` — creates a session record with an associated `ConversationId` foreign key (migration 026). Each ACP session maps to exactly one Zeph conversation.
- `get_acp_session_conversation_id(session_id)` — returns the `ConversationId` for a session, or `None` for legacy sessions created before migration 026.
- `set_acp_session_conversation_id(session_id, conversation_id)` — updates the conversation mapping for an existing session. Used to backfill legacy sessions on first resume.
- `copy_conversation(source, target)` — copies all messages and summaries from one conversation to another within a single transaction, preserving insertion order. Used by `fork_session` to clone history into a new isolated conversation.
- `list_acp_sessions()` — returns all sessions ordered by `created_at DESC` as `Vec<AcpSessionInfo>` (id + created_at). Used by `_session/list` to merge persisted sessions with in-memory state.
- `import_acp_events(session_id, &[(&str, &str)])` — bulk-inserts events inside a single SQLite transaction. All events are written atomically (commit or rollback). Used by `_session/import` for portable session transfer.

> [!NOTE]
> Event cascade delete is handled at the SQL level: deleting a session via `delete_acp_session` removes all associated events.

## Graph memory

The `graph` module provides SQLite-backed entity-relationship tracking:

- **Entities** — named nodes with 8 types (person, tool, concept, project, language, file, config, organization)
- **Typed edges** — 8 relationship types (uses, related_to, part_of, depends_on, created_by, authored, manages, contains) enabling edge-type-aware traversal and filtering
- **Entity canonicalization** — `canonical_name` + alias table prevents duplicates from name variations ("Rust", "rust-lang", "Rust language" resolve to one entity). Alias-first resolution with deterministic first-registered-wins semantics
- **Edges** — directed relationships with bi-temporal timestamps (`valid_from`/`valid_to` for fact validity, `created_at`/`expired_at` for ingestion); `edges_at_timestamp()` returns edges valid at a given point in time, `edge_history()` returns all versions of an edge ordered by `valid_from DESC`, migration 030 adds partial indexes for temporal range queries
- **Communities** — groups of related entities detected via label propagation (petgraph) with LLM-generated summaries
- **Graph eviction** — automatic cleanup of expired edges, orphan entities, and entity cap enforcement via `expired_edge_retention_days` and `max_entities` config
- **BFS traversal** — cycle-safe breadth-first search with configurable hop limit; `bfs_at_timestamp()` variant traverses only edges valid at a given point in time for historical graph queries
- **GraphFact** — retrieval-side type with composite scoring for context injection; includes `valid_from` field for recency-aware scoring when `temporal_decay_rate > 0`
- **`graph_recall`** — query-time retrieval: splits the query into words, matches seed entities via FTS5 full-text index with BM25 ranking (including aliases), runs BFS up to `max_hops`, builds `GraphFact` structs with hop-distance-weighted composite scores, deduplicates by canonical name, and returns the top-K facts for context injection
- **Embedding-based entity resolution** — when `use_embedding_resolution = true`, entities are deduplicated via cosine similarity in Qdrant with a two-threshold approach (auto-merge at >= 0.85, LLM disambiguation at >= 0.70, new entity below); integrated after alias and canonical-name lookup steps; falls back to create-new on failure

`GraphStore` provides CRUD methods over five SQLite tables (`graph_entities`, `graph_entity_aliases`, `graph_edges`, `graph_communities`, `graph_metadata`). Schema is created by migrations 021, 023, and 024.

`SemanticMemory::spawn_graph_extraction()` runs LLM-powered extraction as a fire-and-forget background task with configurable timeout. `recall_graph()` performs fuzzy entity matching plus BFS edge traversal, returning composite-scored `GraphFact` values for context injection.

The `HeuristicRouter` in `zeph-memory` includes a `Graph` route variant: relationship queries (e.g., "related to", "connection between", "opinion on") are automatically routed to `graph_recall`.

Configure via `[memory.graph]` in `config.toml`:

```toml
[memory.graph]
enabled = true
max_hops = 2
recall_limit = 10
extraction_timeout_secs = 15
use_embedding_resolution = true     # semantic entity dedup via Qdrant (default: false)
entity_similarity_threshold = 0.85  # auto-merge threshold
entity_ambiguous_threshold = 0.70   # LLM disambiguation threshold
expired_edge_retention_days = 90    # Days to retain superseded edges
max_entities = 0                    # Max entities cap (0 = unlimited)
temporal_decay_rate = 0.0           # Decay rate for scoring older facts (0.0 = disabled); validated: must be in [0.0, 10.0], not NaN or Inf
edge_history_limit = 100            # Max edge versions returned by edge_history()

[memory.graph.spreading_activation]
enabled = false                     # Enable SYNAPSE spreading activation retrieval
lambda = 0.85                       # Decay factor per hop (energy × lambda at each step)
max_hops = 3                        # Maximum traversal depth from seed entities
max_activated = 50                  # Maximum nodes activated before stopping
timeout_ms = 500                    # Activation timeout to prevent runaway traversal
```

## Importance scoring

Messages are scored at write time via `compute_importance()`. The score is stored in the `importance_score` column (default 0.5 for legacy rows). When `importance_enabled = true` on `SemanticMemory`, recall results are blended with importance scores for content-aware ranking.

| Config field | Type | Default | Description |
|---|---|---|---|
| `importance_enabled` | bool | `false` | Enable importance-blended recall ranking |
| `importance_weight` | f64 | `0.3` | Weight of importance score in the final blend |

## Features

| Feature | Description |
|---------|-------------|
| `experiments` | Experiment result and session summary persistence in SQLite |
| `pdf` | PDF document loading via `pdf-extract` |

## Installation

```bash
cargo add zeph-memory

# With PDF document loading
cargo add zeph-memory --features pdf

# With experiment result persistence
cargo add zeph-memory --features experiments
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
