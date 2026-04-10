---
aliases:
  - Memory Embeddings
  - Embed Backfill
  - Batch Embedding
tags:
  - sdd
  - spec
  - memory
  - embeddings
  - batch
created: 2026-04-10
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[004-1-architecture]]"
  - "[[004-2-compaction]]"
  - "[[004-3-admission-control]]"
  - "[[004-5-temporal-decay]]"
---

# Spec: Memory Embeddings (Backfill & Batch Strategies)

> [!info]
> Semantic embedding generation for memory messages, batch backfill strategies,
> concurrency control, and TUI integration for embedding lifecycle visibility.

## Overview

All admitted messages must be embedded for semantic search. This spec defines how embeddings are
generated, batched, cached, and backfilled when the system boots or recovers from failure.

### Problem Statement

Embedding every message synchronously blocks the agent. Generating embeddings asynchronously
requires careful backpressure handling and recovery semantics to ensure no message is left
unembedded indefinitely.

### Goal

Implement reliable, bounded embedding generation with batch strategies and full visibility
into the embedding pipeline.

---

## Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN message admitted, THE SYSTEM SHALL queue for embedding (async) | must |
| FR-002 | WHEN embedding_batch_size messages queued, THE SYSTEM SHALL embed as batch | must |
| FR-003 | WHEN Qdrant unavailable, THE SYSTEM SHALL retry with exponential backoff | must |
| FR-004 | WHEN system boots, THE SYSTEM SHALL backfill embeddings for unembed messages | must |
| FR-005 | Backfill SHALL respect max_concurrent batches to avoid overwhelming Qdrant | must |
| FR-006 | TUI SHALL display embedding queue depth, backfill progress, batch status | should |
| FR-007 | WHEN compaction applied, embeddings SHALL be regenerated for new summary | must |

---

## Key Invariants

### Always
- A message is "complete" in memory only after: stored in SQLite AND embedded in Qdrant
- Backfill runs at low priority; agent operations take precedence
- Embed queue persists across restarts (via SQLite unembed_at timestamp)
- TUI spinner must show active batch count and queue depth

### Never
- Block agent on embedding I/O
- Batch embeddings across different models or providers
- Lose embed requests on failure — retry until success or explicit purge

---

## Architecture

```
Admission
├─ Message stored in SQLite
├─ Unembed flag set (unembed_at = null)
└─ Queued for embedding

Async Embedding Worker
├─ Collect batch (size = embedding_batch_size)
├─ Resolve embedding provider
├─ Generate embeddings
├─ Upsert into Qdrant
├─ Update SQLite: unembed_at = now
└─ Backpressure: wait if queue > max_backlog

Boot/Recovery
├─ Query SQLite: WHERE unembed_at IS NULL
├─ Spawn backfill worker (max_concurrent batches)
├─ Same batching logic as live embedding
├─ TUI progress bar for backfill
└─ Agent operations proceed concurrently
```

---

## Batch Strategy

### Live Batching (agent running)
- Collect up to `embedding_batch_size` messages
- Wait up to `embedding_batch_timeout_ms` for batch to fill
- Whichever comes first: batch-size OR timeout → emit batch

### Backfill (system boot or recovery)
- Query unembed messages, ordered by `created_at` DESC (newest first)
- Spawn up to `max_concurrent_backfill_batches` worker tasks
- Each worker pulls next batch, embeds, updates SQLite + Qdrant
- Progress tracked per worker and shown in TUI

---

## Concurrency Model

- **Embedding worker**: single background task collecting and batching live messages
- **Backfill workers**: pool of up to N concurrent tasks (N = `max_concurrent_backfill_batches`)
- **SQLite**: serialized writes (SQLite handles locking); reads are concurrent
- **Qdrant**: assume parallel upsert is safe; rate-limit if needed via backpressure

---

## Config

```toml
[memory.embeddings]
enabled = true
embedding_batch_size = 32               # messages per batch
embedding_batch_timeout_ms = 2000       # time to wait for batch to fill
max_backlog = 500                       # pause producer if queue > this
embedding_provider = "fast"             # [[llm.providers]] name for embeddings
max_concurrent_backfill_batches = 4     # parallel workers during boot

[memory.embeddings.retry]
initial_backoff_ms = 500
max_backoff_ms = 30000
max_retries = 5
```

---

## Embedding Provider Selection

- `embedding_provider`: references a provider in `[[llm.providers]]` that supports embeddings
- If unset, fall back to default LLM provider (if it supports embeddings)
- If default provider does not support embeddings, fail startup with clear error

---

## Backfill Recovery Scenarios

| Scenario | Behavior |
|----------|----------|
| Clean boot, no unembed messages | backfill worker exits immediately |
| Boot with 10k unembed messages | backfill runs in background, agent responsive |
| Embedding provider down at boot | backfill retries with exponential backoff, agent starts anyway |
| Qdrant down after 1 batch | backfill pauses, SQLite marked messages as pending, retry on recovery |
| User stops agent mid-backfill | restart continues from next unembed batch |

---

## TUI Integration

Embedding status displayed in:
- **Status bar**: `Embedding: queue=47, batch=1/4, backfill=0%` (when backfilling)
- **Metrics panel**: unembed count, backfill ETA, batch latency
- **Logs**: batch completion, retry attempts, Qdrant errors

---

## Integration Points

- [[004-1-architecture]] — embedding required after admission
- [[004-2-compaction]] — regenerate embeddings after compaction applied
- [[004-3-admission-control]] — embeddings used for relevance scoring
- [[004-5-temporal-decay]] — temporal decay scores may adjust embedding weight
- [[zeph-memory/spec]] — parent system orchestrator

---

## See Also

- [[004-memory/spec]] — Parent
- [[004-1-architecture]] — Core pipeline where embeddings are called
- [[004-2-compaction]] — Triggers re-embedding on summary
