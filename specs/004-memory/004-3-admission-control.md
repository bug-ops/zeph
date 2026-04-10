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
> Adaptive Memory Admission Control (A-MAC): five-factor importance scoring,
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
| FR-003 | Scoring SHALL consider 5 factors: recency, relevance, tool_use, unique_entities, length | must |

---

## Key Invariants

### Always
- Admission check returns `Result<Option<MessageId>>` — None means rejected, not error
- When [memory.admission] enabled=false, ALL messages admitted (pass-through)
- Scoring failure is fail-open — admit on any error

### Never
- Treat None from remember() as an error
- Use admission control as security gate (it's for noise filtering only)

---

## Five-Factor Scoring Model

| Factor | Weight | Calculation |
|--------|--------|-------------|
| Recency | 0.2 | exponential decay from now |
| Relevance | 0.2 | embedding similarity to context |
| Tool Use | 0.2 | 1.0 if contains tool output, 0.0 else |
| Entity Density | 0.2 | unique named entities / message length |
| Message Length | 0.2 | normalized (longer = higher, cap at threshold) |

Final score = sum of weighted factors, range [0.0, 1.0].

---

## Config

```toml
[memory.admission]
enabled = true
threshold = 0.5
weights = { recency = 0.2, relevance = 0.2, tool = 0.2, entities = 0.2, length = 0.2 }
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
