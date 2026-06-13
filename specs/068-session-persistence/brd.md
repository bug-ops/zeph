---
aliases:
  - Session Persistence BRD
  - BRD 068
tags:
  - brd
  - session
  - persistence
created: 2026-06-13
status: draft
related:
  - "[[068-session-persistence/spec]]"
  - "[[068-session-persistence/nfr]]"
issues:
  - "#2807"
  - "#3102"
  - "#3074"
---

# BRD 068 — Session Persistence, Event Log Replay, and `zeph serve`

## 1. Executive Summary

Zeph conversations today are ephemeral: a crash, restart, or channel disconnect destroys all in-progress context. Users lose work, context must be re-established from scratch, and long-running background tasks cannot survive agent restarts. Three related GitHub issues (#2807, #3102, #3074) request a solution: make every conversation durable, replayable, and shareable across multiple simultaneous client connections.

This BRD defines the business case for spec-068, which delivers that solution through three integrated capabilities:
1. Append-only JSONL event log as the source of truth for every conversation.
2. Deterministic replay/resume/fork of any conversation from any point.
3. A persistent background agent service (`zeph serve`) that multiplexes named conversation-sessions.

---

## 2. Stakeholders

| Role | Concern |
|------|---------|
| **End users (CLI/TUI)** | Conversations survive crashes; work is not lost; can resume exactly where they left off |
| **IDE users (ACP)** | Session state is preserved across editor restarts; fork-for-experiment workflow works reliably |
| **Telegram/Discord users** | Bot restarts do not lose conversation context |
| **Operators / power users** | `zeph serve` enables shared agent instances accessible from multiple terminals or tools |
| **Developers building on Zeph** | Deterministic replay enables regression testing of agent behavior; event logs enable debugging |

---

## 3. Business Requirements

### BR-1 — Conversation durability
Every conversation, on any channel (CLI, TUI, ACP, Telegram), must survive a process restart or crash without message loss, provided the process was not killed in the middle of writing a single event line.

**Rationale:** Users frequently run long-running inference tasks. A single crash that destroys context is a data-loss incident.

**Traces to SRS:** SRS-R1, SRS-R2, SRS-R3 (event log write/recovery), SRS-R10 (SessionSink dual-write).

### BR-2 — Deterministic replay
Replaying a saved conversation must reconstruct a context byte-identical to what the live agent had at that point. Tool results, condensation summaries, and compaction outputs must be recorded so replay does not re-execute side-effecting operations.

**Rationale:** Reproducibility is essential for debugging, for regression testing of agent behavior, and for the #3102 feature request (immutable event log).

**Traces to SRS:** SRS-R4 (ReplayEngine), SRS-R5 (no tool re-execution), SRS-R8 (condensation events).

### BR-3 — Fork-at-point
Users must be able to fork a conversation at any recorded event, creating an independent branch that does not affect the original.

**Rationale:** Users experiment with alternative prompts, alternative models, or alternative tool results. Today there is no way to branch without losing the original context. The Codex CLI pattern (cited in #2807) demonstrates this is a valued workflow.

**Traces to SRS:** SRS-R6 (ForkEngine), SRS-R7 (eager copy semantics).

### BR-4 — Multi-channel serve mode
A named conversation-session must be accessible simultaneously from multiple clients (CLI, TUI, HTTP, ACP) without losing state or ordering guarantees.

**Rationale:** Power users want to attach from a TUI, detach, and re-attach from a different terminal without losing the thread. #3074 explicitly requests a persistent background service.

**Traces to SRS:** SRS-R11 (SessionActor), SRS-R12 (LiveSessionRegistry), SRS-R13 (HTTP/SSE API).

### BR-5 — Context condensation as a durable, replayable operation
When a long conversation's context approaches the model's limit, the condensation operation must be recorded in the event log so it can be replayed deterministically. Condensation must not overlap with live compaction on the same sequence range.

**Rationale:** Without this, resuming a very long session either fails (context too large) or produces a non-deterministic reconstructed context (breaks BR-2).

**Traces to SRS:** SRS-R8 (Condensation event), SRS-R9 (INV-SP-4 non-overlap).

### BR-6 — Channel-agnostic session identity
Session identity (the concept of "a named conversation") must work on CLI, TUI, and Telegram, not just on ACP. Existing ACP session infrastructure must be generalized rather than duplicated.

**Rationale:** Today sessions are an ACP-only concept. Non-ACP users get no persistence. Duplicating the infrastructure would create maintenance debt.

**Traces to SRS:** SRS-R14 (non-ACP SessionId minting), SRS-R15 (acp_sessions generalization).

---

## 4. Success Metrics

| Metric | Target |
|--------|--------|
| Message loss on crash | 0 messages lost for complete turns; at most 1 in-flight event lost (torn write) |
| Resume latency (typical session, < 10k events) | < 2 seconds wall time |
| Fork latency (typical session, < 10k events) | < 5 seconds wall time |
| Serve connection concurrency | ≥ 10 simultaneous connections to different sessions, no degradation |
| Condensation non-overlap violations | 0 (enforced by INV-SP-4) |
| Backward compatibility | Existing ACP session tests pass unchanged after delegation |

---

## 5. Constraints

- **Single-process only (MVP):** Multi-host serve or distributed agent clusters are out of scope. A single `zeph serve` instance is the unit.
- **No tool re-execution during replay:** Replay reconstructs state from recorded outputs only (A3). This constrains the replay-to-equivalent-context guarantee to sessions where all tool results are faithfully recorded.
- **Pre-1.0 codebase rules apply:** No backward-compatibility shims beyond what is required; breaking changes to `sessions resume` command semantics are documented in CHANGELOG.
- **Existing `acp_sessions` schema extended, not replaced:** Migration 105 adds columns to the existing table; a rename to `conversation_sessions` is deferred to post-1.0.

---

## 6. Out of Scope

- Re-executing tool side-effects during replay.
- Multi-host or distributed session storage.
- Automatic AEAD encryption of session logs (opt-in only; deferred).
- Copy-on-write fork optimization (eager copy for MVP).
- Retroactive event log synthesis from pre-migration `messages` rows.
- Renaming `acp_sessions` to `conversation_sessions` (cosmetic; post-1.0).
- Cross-session blob sharing.

---

## 7. Dependencies

| Dependency | Nature |
|-----------|--------|
| `zeph-durable` | Design precedent (mirror journal/replay pattern); NOT extended |
| `zeph-acp` | Session handlers delegated to `zeph-session` engines |
| `zeph-context` | `summarize_structured` / `SummarizationDeps` reused by `LlmCondenser` |
| `zeph-agent-persistence` | `SessionSink` dual-write path |
| `zeph-db` | Migration 105 SQLite + PostgreSQL |
| `zeph-gateway` | Auth/rate-limit patterns reused for serve HTTP |
| `zeph-common` | `SessionId` (already present) reused without modification |
