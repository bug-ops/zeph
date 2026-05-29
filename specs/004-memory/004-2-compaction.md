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

---

## Acon: Tool-Result Compression (Added)

**Status: implemented** (commit #4554, closes #4021)

Acon (Adaptive Context Compaction) compresses oversized tool-result messages at the tool-execution
boundary, before they enter the context window. Three handling tiers are applied based on token count:

| Tier | Condition | Behavior |
|---|---|---|
| Pass-through | `tokens < passthrough_threshold` | No compression; result passes as-is |
| Summarize | `passthrough_threshold <= tokens < summarize_threshold` | LLM call produces a summary |
| Budget-cap | `tokens >= summarize_threshold` | Truncate to `total_budget` tokens, then summarize if configured |

Config:

```toml
[memory.compression.acon]
enabled = false
passthrough_threshold = 200    # tokens below which no compression is applied
summarize_threshold = 800      # tokens above which summarization is triggered
total_budget = 4096            # absolute token cap per tool result
acon_provider = ""             # named [[llm.providers]] entry; empty = primary
```

Validation: `passthrough_threshold < summarize_threshold <= total_budget` enforced at startup.

### Deterministic Order

Acon compression applies to tool results in a **deterministic** order (sorted by message index,
not by insertion order or task ID), ensuring reproducible compaction across retries (commit #4578).

### Key Invariants

- Acon runs at the tool-result boundary — NEVER during fidelity scoring or hard compaction
- Summarized results are stored; original oversized text is NOT retained to save tokens
- `enabled = false` is a zero-overhead no-op — no compression, no LLM calls
- Validation prevents `passthrough_threshold >= summarize_threshold` misconfiguration

---

## ARC: Agent-Initiated Compaction (Added)

**Status: implemented** (commit #4554, closes #4020)

ARC (Agent-Requested Compaction) exposes a `request_compaction` internal tool that the LLM
can call when it detects its own context is getting crowded. This is a cooperative compaction
path: the agent opts in.

```toml
[memory.compression]
allow_agent_compaction = false   # opt-in; when true, LLM can call request_compaction
```

When `allow_agent_compaction = true`, the `request_compaction` tool is registered in the
tool catalog. Calling it triggers the same hard-compaction path as the automated 90% threshold
trigger — no new compaction logic is introduced.

### Key Invariants

- `request_compaction` is only available when `allow_agent_compaction = true`
- ARC does not bypass the compaction probe — summary quality is still validated
- The tool is exempt from the adversarial policy gate (it is a cooperative, not injection-triggered, action)

---

## See Also

- [[004-memory/spec]] — Parent: Memory System
- [[004-3-admission-control]] — Admission control after compaction
- [[004-4-embeddings]] — Embedding updates after compaction
