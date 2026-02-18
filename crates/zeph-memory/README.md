# zeph-memory

SQLite-backed conversation persistence with Qdrant vector search.

## Overview

Provides durable conversation storage via SQLite and semantic retrieval through Qdrant vector search. The `SemanticMemory` orchestrator combines both backends, enabling the agent to recall relevant context from past conversations using embedding similarity.

## Key modules

| Module | Description |
|--------|-------------|
| `sqlite` | SQLite storage for conversations and messages |
| `qdrant` | Qdrant client for vector upsert and search |
| `qdrant_ops` | `QdrantOps` — high-level Qdrant operations |
| `semantic` | `SemanticMemory` — orchestrates SQLite + Qdrant |
| `types` | `ConversationId`, `MessageId`, shared types |
| `error` | `MemoryError` — unified error type |

**Re-exports:** `MemoryError`, `QdrantOps`, `ConversationId`, `MessageId`

## Usage

```toml
[dependencies]
zeph-memory = { path = "../zeph-memory" }
```

## License

MIT
