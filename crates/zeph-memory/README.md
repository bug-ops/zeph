# zeph-memory

[![Crates.io](https://img.shields.io/crates/v/zeph-memory)](https://crates.io/crates/zeph-memory)
[![docs.rs](https://img.shields.io/docsrs/zeph-memory)](https://docs.rs/zeph-memory)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Semantic memory with SQLite and Qdrant for Zeph agent.

## Overview

Provides durable conversation storage via SQLite and semantic retrieval through Qdrant vector search (or embedded SQLite vector backend). The `SemanticMemory` orchestrator combines both backends, enabling the agent to recall relevant context from past conversations using embedding similarity.

Recall quality is enhanced by MMR (Maximal Marginal Relevance) re-ranking for result diversity and temporal decay scoring for recency bias. Both are configurable via `SemanticConfig`.

Includes a document ingestion subsystem for loading, chunking, and storing user documents (text, Markdown, PDF) into Qdrant for RAG workflows.

## Key modules

| Module | Description |
|--------|-------------|
| `sqlite` | SQLite storage for conversations and messages |
| `sqlite::history` | Input history persistence for CLI channel |
| `qdrant` | Qdrant client for vector upsert and search |
| `qdrant_ops` | `QdrantOps` — high-level Qdrant operations |
| `semantic` | `SemanticMemory` — orchestrates SQLite + Qdrant |
| `document` | Document loading, splitting, and ingestion pipeline |
| `document::loader` | `TextLoader` (.txt/.md), `PdfLoader` (feature-gated: `pdf`) |
| `document::splitter` | `TextSplitter` with configurable chunking |
| `document::pipeline` | `IngestionPipeline` — load, split, embed, store via Qdrant |
| `vector_store` | `VectorStore` trait and `VectorPoint` types |
| `sqlite_vector` | `SqliteVectorStore` — embedded SQLite-backed vector search as zero-dependency Qdrant alternative |
| `embedding_store` | `EmbeddingStore` — high-level embedding CRUD |
| `embeddable` | `Embeddable` trait and `EmbeddingRegistry<T>` — generic Qdrant sync/search for any embeddable type |
| `types` | `ConversationId`, `MessageId`, shared types |
| `error` | `MemoryError` — unified error type |

**Re-exports:** `MemoryError`, `QdrantOps`, `ConversationId`, `MessageId`, `Document`, `DocumentLoader`, `TextLoader`, `TextSplitter`, `IngestionPipeline`, `Chunk`, `SplitterConfig`, `DocumentError`, `DocumentMetadata`, `PdfLoader` (behind `pdf` feature), `Embeddable`, `EmbeddingRegistry`

## Ranking options

| Option | Config field | Default | Description |
|--------|-------------|---------|-------------|
| MMR re-ranking | `semantic.mmr_enabled` | `false` | Post-retrieval diversity via Maximal Marginal Relevance |
| MMR lambda | `semantic.mmr_lambda` | `0.7` | Balance between relevance (1.0) and diversity (0.0) |
| Temporal decay | `semantic.temporal_decay_enabled` | `false` | Time-based score attenuation favoring recent memories |
| Decay half-life | `semantic.temporal_decay_half_life_days` | `30` | Days until a memory's score drops to 50% |

## Features

| Feature | Description |
|---------|-------------|
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
