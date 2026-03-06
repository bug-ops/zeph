# zeph-memory

[![Crates.io](https://img.shields.io/crates/v/zeph-memory)](https://crates.io/crates/zeph-memory)
[![docs.rs](https://img.shields.io/docsrs/zeph-memory)](https://docs.rs/zeph-memory)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Semantic memory with SQLite and Qdrant for Zeph agent.

## Overview

Provides durable conversation storage via SQLite and semantic retrieval through Qdrant vector search (or embedded SQLite vector backend). The `SemanticMemory` orchestrator combines both backends, enabling the agent to recall relevant context from past conversations using embedding similarity.

Recall quality is enhanced by MMR (Maximal Marginal Relevance) re-ranking for result diversity and temporal decay scoring for recency bias. Both are configurable via `SemanticConfig`.

Query-aware memory routing (`MemoryRouter` trait, `HeuristicRouter` default) classifies each query as Keyword (SQLite FTS5), Semantic (Qdrant), or Hybrid and dispatches accordingly. Configure via `[memory.routing]`.

Includes a document ingestion subsystem for loading, chunking, and storing user documents (text, Markdown, PDF) into Qdrant for RAG workflows.

## Key modules

| Module | Description |
|--------|-------------|
| `sqlite` | SQLite storage for conversations, messages, and user corrections (`zeph_corrections` table, migration 018 adds `outcome_detail` column); visibility-aware queries (`load_history_filtered` via CTE, `messages_by_ids`, `keyword_search`); durable compaction via `replace_conversation()`; composite covering index `(conversation_id, id)` on messages for efficient history reads |
| `sqlite::history` | Input history persistence for CLI channel |
| `sqlite::acp_sessions` | ACP session and event persistence for session resume and lifecycle tracking |
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
| `embedding_store` | `EmbeddingStore` — high-level embedding CRUD |
| `embeddable` | `Embeddable` trait and `EmbeddingRegistry<T>` — generic Qdrant sync/search for any embeddable type |
| `types` | `ConversationId`, `MessageId`, shared types |
| `token_counter` | `TokenCounter` — tiktoken-based (cl100k_base) token counting with DashMap cache (10k cap), OpenAI tool schema formula, 64KB input guard with chars/4 fallback |
| `routing` | `MemoryRouter` trait and `HeuristicRouter` — query-aware routing to Keyword, Semantic, or Hybrid backends |
| `sqlite::graph_store` | `RawGraphStore` trait and `SqliteGraphStore` — raw JSON-blob persistence for task orchestration graphs (save/load/list/delete); `GraphSummary` metadata type; used by `zeph-core::orchestration::GraphPersistence` for typed serialization (feature-gated: `orchestration`) |
| `graph` | `GraphStore`, `Entity`, `EntityAlias`, `Edge`, `Community`, `GraphFact`, `EntityType` — knowledge graph with BFS traversal and entity canonicalization (feature-gated: `graph-memory`) |
| `graph::extractor` | `GraphExtractor` — LLM-powered entity/relation extraction via structured output; `EntityResolver` for dedup and supersession (feature-gated: `graph-memory`) |
| `graph::retrieval` | `graph_recall` — query-time graph retrieval: fuzzy entity matching (including aliases), BFS from seed entities, composite scoring, canonical-name deduplication (feature-gated: `graph-memory`) |
| `error` | `MemoryError` — unified error type |

**Re-exports:** `MemoryError`, `QdrantOps`, `ConversationId`, `MessageId`, `Document`, `DocumentLoader`, `TextLoader`, `TextSplitter`, `IngestionPipeline`, `Chunk`, `SplitterConfig`, `DocumentError`, `DocumentMetadata`, `PdfLoader` (behind `pdf` feature), `Embeddable`, `EmbeddingRegistry`, `ResponseCache`, `MemorySnapshot`, `TokenCounter`, `UserCorrection`, `FeedbackDetector`

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

`SqliteStore` provides persistence for ACP session lifecycle and event replay. Two methods added for custom method support:

- `list_acp_sessions()` — returns all sessions ordered by `created_at DESC` as `Vec<AcpSessionInfo>` (id + created_at). Used by `_session/list` to merge persisted sessions with in-memory state.
- `import_acp_events(session_id, &[(&str, &str)])` — bulk-inserts events inside a single SQLite transaction. All events are written atomically (commit or rollback). Used by `_session/import` for portable session transfer.

> [!NOTE]
> Event cascade delete is handled at the SQL level: deleting a session via `delete_acp_session` removes all associated events.

## Graph memory

When the `graph-memory` feature is enabled, the `graph` module provides SQLite-backed entity-relationship tracking:

- **Entities** — named nodes with 8 types (person, tool, concept, project, language, file, config, organization)
- **Entity canonicalization** — `canonical_name` + alias table prevents duplicates from name variations ("Rust", "rust-lang", "Rust language" resolve to one entity). Alias-first resolution with deterministic first-registered-wins semantics
- **Edges** — directed relationships with bi-temporal timestamps (`valid_from`/`valid_to` for fact validity, `created_at`/`expired_at` for ingestion)
- **Communities** — groups of related entities with LLM-generated summaries
- **BFS traversal** — cycle-safe breadth-first search with configurable hop limit
- **GraphFact** — retrieval-side type with composite scoring for context injection
- **`graph_recall`** — query-time retrieval: splits the query into words, matches seed entities via FTS5 full-text index with BM25 ranking (including aliases), runs BFS up to `max_hops`, builds `GraphFact` structs with hop-distance-weighted composite scores, deduplicates by canonical name, and returns the top-K facts for context injection

`GraphStore` provides CRUD methods over five SQLite tables (`graph_entities`, `graph_entity_aliases`, `graph_edges`, `graph_communities`, `graph_metadata`). Schema is created by migrations 021, 023, and 024, and is always present regardless of feature flag.

`SemanticMemory::spawn_graph_extraction()` runs LLM-powered extraction as a fire-and-forget background task with configurable timeout. `recall_graph()` performs fuzzy entity matching plus BFS edge traversal, returning composite-scored `GraphFact` values for context injection.

The `HeuristicRouter` in `zeph-memory` includes a `Graph` route variant: relationship queries (e.g., "related to", "connection between", "opinion on") are automatically routed to `graph_recall` when the `graph-memory` feature is enabled.

Configure via `[memory.graph]` in `config.toml`:

```toml
[memory.graph]
enabled = true
max_hops = 2
recall_limit = 10
extraction_timeout_secs = 15
```

## Features

| Feature | Description |
|---------|-------------|
| `graph-memory` | Knowledge graph with entity-relationship tracking and BFS traversal |
| `orchestration` | Task graph persistence via `SqliteGraphStore` (used by `zeph-core` orchestration) |
| `pdf` | PDF document loading via `pdf-extract` |
| `mock` | In-memory `VectorStore` implementation for testing |

## Installation

```bash
cargo add zeph-memory

# With PDF support
cargo add zeph-memory --features pdf
```

## License

MIT
