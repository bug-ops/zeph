---
aliases:
  - Memory Architecture
  - Core Pipeline
  - Dual Backend
tags:
  - sdd
  - spec
  - memory
  - architecture
  - contract
created: 2026-04-10
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[004-2-compaction]]"
  - "[[004-3-admission-control]]"
  - "[[012-graph-memory/spec]]"
  - "[[031-database-abstraction/spec]]"
---

# Spec: Memory Architecture (Core Pipeline)

> [!info]
> Foundational memory system: SQLite + Qdrant dual backend, semantic response cache,
> message storage, and conversation history tracking.

## Overview

This atomic note documents the **core memory pipeline** — the fundamental architecture that other memory subsystems build upon.

### Problem Statement

Zeph agents need persistent conversation history with semantic search capabilities.
Relational storage (SQLite) provides strong ACID guarantees and efficient querying;
vector storage (Qdrant) enables fast semantic retrieval.

### Goal

Implement a dual-backend memory system that maintains conversation history in SQLite
while building semantic vectors in Qdrant for recall operations.

---

## Architecture

### Dual Backend Strategy

| Backend | Purpose | Guarantees |
|---------|---------|-----------|
| **SQLite** | Relational history, message metadata | ACID, normalized schema |
| **Qdrant** | Semantic vector search | High-dimensional ANN, exact cosine |

### Key Invariants

### Always
- **Messages are never deleted** — only marked compacted or summarized
- **System message is always `messages[0]`** — rebuilt each turn from config + skills
- **Both backends stay consistent** — write to both or none (transactional)

### Ask First
- Adding new message types that change serialization
- Changing Qdrant embedding dimension

### Never
- Store unencrypted secrets in message history
- Block memory operations on persistence errors (fail open)

---

## Message Storage Schema

```sql
-- Core conversation history
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    session_id TEXT,
    kind TEXT,  -- system, user, assistant, tool_output, thinking
    content TEXT,
    timestamp INTEGER,
    compacted_at INTEGER,  -- when this message was summarized
    metadata JSON
);
```

---

## Integration Points

- [[012-graph-memory/spec]] — graph memory reads/writes to message history
- [[004-2-compaction]] — deferred summaries applied to messages
- [[004-3-admission-control]] — admission scoring on remember()

---

## See Also

- [[004-memory/spec]] — Parent: all memory subsystems
- [[031-database-abstraction/spec]] — PostgreSQL alternative backend
