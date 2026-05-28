---
aliases:
  - CAM BRD
tags:
  - brd
  - context
  - memory
created: 2026-05-28
status: approved
related:
  - "[[062-context-adaptive-memory/spec]]"
---

# Context-Adaptive Memory — Business Requirements Document

## 1. Problem Statement

Zeph's context window management currently operates reactively: compaction fires only after the budget is nearly exhausted (typically >90% full). At that point, the system has no choice but to discard or summarize large portions of conversation history without regard to their relevance to the current task.

This leads to three documented failure modes in long sessions:

1. **Context blowout mid-task** — the agent loses essential context (tool outputs, intermediate reasoning, correction messages) precisely when it needs them most.
2. **Uniform discard** — all non-recent messages are treated identically regardless of whether they contain a critical file path established three turns ago or a generic greeting from the start of the session.
3. **Wasted recovery overhead** — after compaction, the agent spends 1–3 additional turns re-establishing context that should have been preserved.

## 2. Business Value

| Benefit | Target Metric |
|---|---|
| Token reduction | 40–60% reduction in context window usage for sessions > 20 turns |
| Fewer context reloads | Eliminate reactive compaction mid-task in > 80% of long sessions |
| Preserved structural history | Tool-use/tool-result pairs and correction messages survive context pressure |
| No behavioral regression | Existing sessions unaffected when feature disabled (`enabled = false`) |

## 3. Stakeholders

| Stakeholder | Role | Interest |
|---|---|---|
| Zeph end users (long session) | Primary beneficiary | Coherent multi-turn sessions without context amnesia |
| Agent loop (`zeph-core`) | Consumer | Receives a better-managed context window |
| Context manager (`zeph-context`) | Owner | Hosts the fidelity scorer and regrade trigger |
| Orchestration (`zeph-orchestration`) | Data source | Provides DAG lookahead hints (deferred in MVP) |

## 4. Business Constraints

- **No external infrastructure required** for MVP: heuristic scoring uses keyword overlap, not embeddings.
- **Off by default** (`enabled = false`) — the feature must not affect existing users until opted in via config.
- **No breaking API changes** to `MessageMetadata` serialization format that would corrupt existing session databases.
- **Latency budget**: context scoring must add < 2ms per turn to the context preparation path.
- **Pre-v1.0**: no backward-compatibility shims required; clean implementation preferred over incremental migration.

## 5. Success Criteria

The initiative is considered successful when:

1. All 12 acceptance criteria in `spec.md §11` pass.
2. A live test session of 30+ turns shows no mid-task context blowout.
3. Token usage per turn is measurably reduced compared to a baseline session without CAM.
4. No regression in existing unit test suite (`cargo nextest run --workspace --lib --bins`).
