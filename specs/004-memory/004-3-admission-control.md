---
aliases:
  - Admission Control
  - A-MAC
  - Importance Scoring
tags:
  - sdd
  - spec
  - memory
  - admission
created: 2026-04-10
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[004-1-architecture]]"
  - "[[004-2-compaction]]"
  - "[[004-4-embeddings]]"
---

# Spec: Memory Admission Control (A-MAC & Importance Scoring)

> [!info]
> Adaptive Memory Admission Control (A-MAC): six-factor importance scoring,
> admission gates, and graceful degradation.

## Overview

Not all messages should be stored in memory. A-MAC scores importance and decides
whether to admit messages based on recency, relevance, and utility.

### Goal

Implement adaptive admission control that filters noise while preserving critical context.

---

## Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN remember() called, THE SYSTEM SHALL score message importance | must |
| FR-002 | WHEN score < admission_threshold, message rejected (returns None) | must |
| FR-003 | Scoring SHALL consider 6 factors: recency, relevance, tool_use, unique_entities, length, frequency | must |

---

## Key Invariants

### Always
- Admission check returns `Result<Option<MessageId>>` — None means rejected, not error
- When [memory.admission] enabled=false, ALL messages admitted (pass-through)
- Scoring failure is fail-open — admit on any error
- A-MAC must use `effective_embed_provider()` for all embedding calls — never the primary conversational provider directly. This applies to both the admission `evaluate()` path and the A-MAC bootstrap fallback. `effective_embed_provider()` resolves: first `[[llm.providers]]` entry with `embed = true`, then first with `embedding_model`, then primary.
- All embedding call sites in semantic submodules (`summarization.rs`, `cross_session.rs`, `recall.rs`) must call `effective_embed_provider()`, not `provider` — this is enforced by `#3035` and `#3154`/`#3162`.

### Never
- Treat None from remember() as an error
- Use admission control as security gate (it's for noise filtering only)
- Call `provider.embed()` directly in any memory submodule — always use `effective_embed_provider()`

---

## Six-Factor Scoring Model

| Factor | Weight | Calculation |
|--------|--------|-------------|
| Recency | 0.1667 | exponential decay from now |
| Relevance | 0.1667 | embedding similarity to context |
| Tool Use | 0.1667 | 1.0 if contains tool output, 0.0 else |
| Entity Density | 0.1667 | unique named entities / message length |
| Message Length | 0.1667 | normalized (longer = higher, cap at threshold) |
| Frequency | 0.1667 | entity mention count with exponential decay |

Final score = sum of weighted factors, range [0.0, 1.0].

### Frequency Factor

The frequency factor tracks how often entities mentioned in the message have been referenced in recent sessions. Calculation:
- Query entity graph for each unique entity in the message
- Count mentions in last N sessions (configurable, default: 10 sessions)
- Apply exponential decay: `count × exp(-λ × days_since_last_mention)` where λ = 0.01
- Normalize to [0, 1] by dividing by threshold (default: 5 mentions)

---

## Config

```toml
[memory.admission]
enabled = true
threshold = 0.5
weights = { recency = 0.1667, relevance = 0.1667, tool = 0.1667, entities = 0.1667, length = 0.1667, frequency = 0.1667 }

[memory.admission.frequency]
# Frequency factor configuration
mention_lookback_sessions = 10
mention_decay_rate = 0.01
mention_normalization_cap = 5.0
```

---

## Integration Points

- [[004-1-architecture]] — called in remember() method
- [[004-2-compaction]] — scores before compaction decision
- [[004-4-embeddings]] — uses embeddings for relevance factor

---

## See Also

- [[004-memory/spec]] — Parent
- [[004-1-architecture]] — Core pipeline where admission is checked
