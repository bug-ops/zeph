# SleepGate: Automatic Memory Forgetting

Over time, the vector index accumulates stale or low-value embeddings that dilute recall quality. SleepGate implements a periodic forgetting pass inspired by memory consolidation during sleep: it scans stored embeddings, scores them on multiple signals, then soft-deletes entries below a retention threshold.

## How It Works

SleepGate runs on a configurable interval (default: every 24 hours). Each pass:

1. Loads candidate embeddings from the vector index
2. Scores each candidate on three signals:
   - **Recency** — when the embedding was last written or accessed
   - **Access frequency** — how often the embedding appeared in recall results
   - **Semantic density** — how many other embeddings are semantically close (high density = redundant)
3. Computes a composite retention score from the three signals
4. Soft-deletes entries below `retention_threshold`

Soft-deleted entries are marked in SQLite and removed from the vector index, but the underlying data remains in SQLite. They can be restored manually if needed.

## Compression Predictor

Before deleting a candidate, SleepGate runs a performance-floor compression predictor that estimates whether removing the embedding would degrade recall quality for recent queries. The predictor replays the last N queries against the index with and without the candidate and measures the recall delta.

Entries flagged as **load-bearing** by the predictor are preserved regardless of their retention score. This prevents SleepGate from removing embeddings that are infrequently accessed but critical for specific query patterns.

## Configuration

```toml
[memory.forgetting]
enabled = true
interval_secs = 86400          # Run every 24 hours (default)
retention_threshold = 0.30     # Composite score below which entries are forgotten (default: 0.30)
```

### Tuning Guidelines

| Scenario | Adjustment |
|----------|------------|
| High-volume sessions (100+ messages/day) | Lower `interval_secs` to `43200` (12h) and raise `retention_threshold` to `0.40` |
| Long-lived agent with years of history | Keep defaults — SleepGate naturally favors recent, frequently-accessed entries |
| Small dataset (<1000 embeddings) | Disable SleepGate — the overhead is not worth it for small indices |
| Recall quality degraded after forgetting | Lower `retention_threshold` to `0.20` to be more conservative |

## Interaction with Other Memory Features

- **A-MAC (Admission Control)**: A-MAC gates writes, SleepGate gates retention. Together they keep the vector index lean on both ends.
- **MemScene Consolidation**: MemScene groups related messages into scene embeddings before SleepGate runs, so individual message embeddings that have been consolidated into scenes are naturally low-scoring and get cleaned up.
- **Temporal Decay**: Temporal decay attenuates recall scores at query time; SleepGate removes entries permanently. They complement each other — decay handles short-term relevance, SleepGate handles long-term hygiene.

## Monitoring

Check SleepGate activity in the logs:

```bash
RUST_LOG=zeph_memory=debug zeph --config config.toml 2>&1 | grep -i sleep
```

The `zeph memory stats` command shows the total embedding count and the number of soft-deleted entries.

## Next Steps

- [Memory and Context](../concepts/memory.md) — overview of the memory system
- [Set Up Semantic Memory](../guides/semantic-memory.md) — vector backend setup
- [Context Engineering](context.md) — compaction and budget management
