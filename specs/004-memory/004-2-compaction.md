---
aliases:
  - Compaction
  - Deferred Summaries
  - Tool Pair Summarization
tags:
  - sdd
  - spec
  - memory
  - compaction
created: 2026-04-10
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[004-1-architecture]]"
  - "[[004-3-admission-control]]"
  - "[[004-4-embeddings]]"
---

# Spec: Memory Compaction (Deferred Summaries & Probe)

> [!info]
> Tool pair summarization, compaction probe validation, soft/hard eviction thresholds,
> and context pressure management.

## Overview

Compaction reduces token usage by summarizing tool output pairs (request + response)
when context pressure rises. This spec defines **deferred** summaries (applied on demand)
and the **compaction probe** validation mechanism.

### Problem Statement

Large tool outputs (code snippets, API responses) quickly consume context tokens.
Simply deleting them loses information. Summarization preserves semantics at lower cost.

### Goal

Implement deferred tool pair summaries that compress on-demand at context pressure points.

---

## Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN context usage > 60%, THE SYSTEM SHALL mark tool pairs for summary | must |
| FR-002 | WHEN context usage > 90%, THE SYSTEM SHALL apply summaries before LLM call | must |
| FR-003 | WHEN compaction_provider set, THE SYSTEM SHALL use that provider for summaries | should |
| FR-004 | Compaction probe SHALL validate summary quality before injection | must |

---

## Key Invariants

### Always
- Tool pair summaries are stored in `compacted_at` field — never remove, only update
- Soft threshold (~60%) marks for later; hard threshold (~90%) applies now
- Compaction probe must verify summary semantic loss < threshold

### Never
- Apply summaries eagerly — only on context pressure or explicit request
- Lose the original tool output — store summary alongside

---

## Architecture

```
Context Pressure Check
├─ Soft Threshold (~60%)
│  └─ Mark tool pairs compacted_at = now
│
└─ Hard Threshold (~90%)
   └─ Apply marked summaries before LLM call
      └─ Compaction Probe validates
         └─ Semantic distance < threshold → inject
         └─ Otherwise → truncate tool output
```

## Config

```toml
[memory.compaction]
enabled = true
soft_threshold_percent = 60
hard_threshold_percent = 90
compaction_provider = "fast"  # references [[llm.providers]]
probe_semantic_threshold = 0.85
```

---

## Integration Points

- [[004-1-architecture]] — applied during message recall
- [[002-agent-loop/spec]] — checked on context pressure
- [[003-llm-providers/spec]] — uses named provider for summaries

---

## See Also

- [[004-memory/spec]] — Parent: Memory System
- [[004-3-admission-control]] — Admission control after compaction
- [[004-4-embeddings]] — Embedding updates after compaction
