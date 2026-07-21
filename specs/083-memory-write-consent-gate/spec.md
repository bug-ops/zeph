---
aliases:
  - MemGhost
  - Write-Time Memory Consent Gate
  - Memory Write Consent Gate
tags:
  - sdd
  - spec
  - memory
  - security
  - consent
created: 2026-07-22
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-9-memory-write-gate]]"
  - "[[039-background-task-supervisor/spec]]"
  - "[[001-system-invariants/spec]]"
---

# Spec: Write-Time Memory Consent Gate (MemGhost)

> [!info]
> Write-time provenance tagging, interactive confirmation, and audit logging for durable
> memory writes derived from untrusted content (tool output, web scrapes, MCP/A2A responses).
> Closes the disclosed **MemGhost** attack pattern. Complements #3960 (retrieval-time trust
> filtering) without implementing its scope. Resolves GitHub issue
> [#6490](https://github.com/bug-ops/zeph/issues/6490).

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-config/src/memory/consent_gate.rs` | `ConsentGateConfig` — `[memory.consent_gate]` |
| `crates/zeph-core/src/memory_tools.rs` | `MemoryToolExecutor`, `MemoryConsentTrustSlot`, interactive confirm path |
| `crates/zeph-core/src/agent/tool_execution/sanitize.rs` | `sanitize_tool_output`, `ratchet_memory_consent_trust_for_dispatch` |
| `crates/zeph-core/src/agent/persistence/store.rs` | Background tool-output write path, disclosure note, audit gating |
| `crates/zeph-sanitizer/src/types.rs` | `ContentSourceKind`, `ContentTrustLevel` (reused taxonomy) |
| `crates/zeph-tools/src/audit.rs` | `AuditEntry::memory_write` constructor |
| `crates/zeph-agent-context/src/state.rs`, `crates/zeph-llm/src/provider.rs` | `MessageMetadata::trust_level` tag, `Agent::context_max_trust_level` |

---

## 1. Overview

### Problem Statement (MemGhost)

Content from autonomous tool output — web scrape, MCP results, batched tool calls — could
persist into durable memory (SQLite + Qdrant) with no provenance tag, no consent, and no
visible disclosure. Once written, untrusted external content becomes indistinguishable from
trusted context in every future session: a prompt-injection payload planted via a scraped page
could be recalled turns or sessions later as if it were the user's own words.

### Goal

Every durable memory write carries write-time provenance (`ContentSourceKind` +
`ContentTrustLevel`, reusing the existing `zeph-sanitizer` taxonomy). Writes derived from
content at or above a configurable trust threshold are gated: the interactive `memory_save`
tool path requires `Channel::confirm`; autonomous background tool-output writes never block
(per the project's non-blocking contract — see [[039-background-task-supervisor/spec]]) but
emit a visible in-turn disclosure note instead. Every write, gated or not, is recorded in the
audit log with source attribution when `audit_all = true`.

### Relationship to [[004-9-memory-write-gate|MemReader Write Quality Gate]]

Two independently-composed gates exist on the memory write path, in this order:

1. **A-MAC admission** ([[004-3-admission-control]]) — importance scoring
2. **MemReader quality gate** ([[004-9-memory-write-gate]]) — content-quality scoring
   (redundancy, reference completeness, contradiction) — a **noise-control** mechanism
3. **MemGhost consent gate** (this spec) — provenance/trust gating — a **security/consent**
   mechanism

They are orthogonal and must not be conflated: MemReader asks "is this write worth keeping?";
MemGhost asks "does the user/operator know and consent that this write came from untrusted
content?" A write can pass MemReader's quality bar while still requiring MemGhost confirmation
(a high-quality fact scraped from an adversarial web page), and vice versa. [[004-9-memory-write-gate]]'s
own "Never" section ("Use quality gate as a security filter") is the reason this separate gate
exists — MemReader was explicitly scoped to exclude consent/security semantics.

### Out of Scope

- Retrieval-time trust filtering (tracked separately as #3960; this spec is write-time only)
- Replacing or altering [[004-9-memory-write-gate]]'s quality scoring
- Path-based trust inference (e.g. treating user-authored instruction files as `Trusted`) —
  `ContentSourceKind::InstructionFile` is treated as `LocalUntrusted` by default; a Phase 2 concern

---

## 2. Mechanism

### Write-Time Provenance

`ContentSourceKind` (`ToolResult`, `WebScrape`, `McpResponse`, `A2aMessage`, `MemoryRetrieval`,
`InstructionFile`, channel message, …) and `ContentTrustLevel` (`Trusted` < `LocalUntrusted` <
`ExternalUntrusted`) are computed once by `sanitize_tool_output` and threaded through
`PersistenceService`/`SemanticMemory::remember*` into nullable `messages.source_kind` /
`trust_level` columns (migration 114) and a Qdrant payload field. `NULL` means "provenance not
recorded" (legacy rows, not-yet-migrated writers) — **never** interpreted as trusted.

### Interactive Path: `memory_save`

The interactive `memory_save` tool requires `Channel::confirm` (via the existing
`ToolError::ConfirmationRequired`/`handle_confirmation_phase` protocol) when the current turn's
tool output reached `confirm_threshold` (default `external_untrusted`). Tracked via a
turn-scoped `MemoryConsentTrustSlot` (`Arc<RwLock<u8>>`, the `ContentTrustLevel` discriminant)
shared between `Agent` and `MemoryToolExecutor` — the latter has no `&Agent` access (the
`ToolExecutor` trait is deliberately object-safe). The model-supplied `role` field on a
`memory_save` call is never trusted as a provenance signal.

### Background Path: Autonomous Tool-Output Writes

Autonomous background tool-output writes never block on `Channel::confirm` — instead, when the
batch's trust reaches `disclose_threshold` (default `local_untrusted`), a visible in-turn
channel disclosure note is emitted (non-blocking, fire-and-forget on the persistence path).

### Audit Logging

Every write (trusted or not) is recorded via `AuditEntry::memory_write` (`source_kind`/
`trust_level` fields, reuses `ClaimSource::Memory` and the existing JSONL audit sink) when
`audit_all = true` (default). Gated by `audit_all` consistently on **both** the interactive
(`MemoryToolExecutor`) and background (`persist_message_inner`) paths — see Follow-up Fixes
below for the bug where this initially diverged between paths.

---

## 3. Follow-up Fixes (Same Feature, Sequential PRs)

The initial implementation (#6544, closes #6490) shipped the mechanism above. Three follow-up
PRs closed defects in the same threat class before the feature reached its current, correct
state — all three are part of this spec's invariants, not separate features.

### #6598 — TOCTOU Bypasses Across Turns, Tiers, and Reload

The turn-scoped trust slot was read at points that did not yet reflect all untrusted content
already present or concurrently arriving in the current turn:

- **Cross-turn deferral (#6558)**: `begin_turn` hard-reset the trust slot to `Trusted` every
  turn, so untrusted tool output fetched in turn N was invisible to a `memory_save` dispatched
  in turn N+1, even though the untrusted content was still live in context. Fixed by tagging
  tool-result batch messages with a persisted `trust_level` (`MessageMetadata::trust_level`,
  the worst-case tier) and scanning the live context (`Agent::context_max_trust_level`) for any
  still-present tag before every tool dispatch.
- **Same-tier/cross-tier parallel dispatch race (#6569)**: the slot was only ratcheted up
  inside `sanitize_tool_output`, which runs *after* a tier's tool calls have already executed
  concurrently via `join_all`. A `memory_save` dispatched in the same batch as `web_scrape` (or
  `memory_search`) could read a stale `Trusted` value during its own concurrent dispatch. Fixed
  by precomputing the batch's worst-case trust tier from tool names alone
  (`Agent::ratchet_memory_consent_trust_for_dispatch`) and writing it to the slot before any
  tool in the batch starts executing.
- **Cross-process reload residual**: `load_history`/`load_history_filtered`/`message_by_id`
  hardcoded `trust_level: None` on reload, silently dropping the gate's context tag across a
  daemon/serve/ACP restart. Fixed by restoring `MessageMetadata::trust_level` from the
  persisted column on every reload path (fail-safe: an unrecognized persisted value maps to the
  most conservative tier, never to `Trusted`).
- **Compaction/summarization residual**: condensing an untrusted tool-result batch into an LLM
  summary (`compact_context`) or a deferred tool-pair summary previously produced an untagged
  summary message/row. Fixed by propagating the worst-case trust tier of the condensed messages
  onto both the in-memory summary message and the persisted summary row.

### #6599 — Provenance Mislabeling and Inconsistent Audit Gating

- A batch's write-time `source_kind` was derived via a lossy heuristic keyed only on aggregated
  `trust_level` (a two-way `if`/`else`), mislabeling MCP responses, A2A messages,
  memory-retrieval replays, and channel messages all as `web_scrape`. Fixed:
  `Agent::sanitize_tool_output` now returns the real per-call `ContentSourceKind` alongside
  `ContentTrustLevel`, tracked directly (tie-break: later call in the batch wins on equal trust
  tier) rather than re-derived from the trust level.
- `consent_gate.audit_all = false` was honored on the background tool-output write path but
  **not** on the interactive `memory_save` path — setting it to `false` did not suppress
  interactive audit entries as documented. Fixed: `MemoryToolExecutor` gates its audit block on
  `audit_all` consistently with the background path, wired through all four entry points
  (CLI/TUI, ACP, A2A daemon, `/sessions*` server).

---

## 4. Config

```toml
[memory.consent_gate]
enabled = true
confirm_threshold = "external_untrusted"   # trusted | local_untrusted | external_untrusted
disclose_threshold = "local_untrusted"
audit_all = true
```

Wired through `AgentSessionConfig` (all four agent entry points: CLI/TUI, ACP, A2A daemon,
`/sessions*` server), the `--init` wizard, and `--migrate-config`.

---

## 5. Key Invariants

- Interactive `memory_save` MUST request `Channel::confirm` when the live-context trust tier is
  at or above `confirm_threshold` — the model-supplied `role` field is NEVER a substitute for
  the tracked trust tier
- Background tool-output writes MUST NEVER block on `Channel::confirm` — only the interactive
  `memory_save` path may block; background writes use the non-blocking disclosure-note path
- `MemoryConsentTrustSlot` MUST be ratcheted up **before** any tool in a dispatch batch starts
  executing (`ratchet_memory_consent_trust_for_dispatch`), not after — ratcheting after
  `join_all` resolves reopens the #6569 same-batch race
- `MessageMetadata::trust_level` MUST be restored on every history-reload path
  (`load_history`/`load_history_filtered`/`message_by_id`) and propagated through both
  compaction and deferred tool-pair summarization — dropping it on any of these paths
  reopens a #6598-class bypass
- An unrecognized/missing persisted `trust_level` MUST map to the most conservative tier
  (`ExternalUntrusted`), never to `Trusted` — fail-safe, not fail-open, for this gate
  specifically (contrast with [[004-9-memory-write-gate]]'s scoring, which is fail-open)
- `source_kind` MUST be tracked directly from `sanitize_tool_output`'s per-call classification,
  NEVER re-derived from the aggregated `trust_level` alone
- `consent_gate.audit_all` MUST gate both the interactive (`MemoryToolExecutor`) and background
  (`persist_message_inner`) audit-logging call sites identically
- `NULL`/unrecorded `source_kind`/`trust_level` on a legacy row is never treated as `Trusted`

### Never

- Use the model-supplied `role` field as a provenance/trust signal
- Block the background tool-output write path on `Channel::confirm`
- Treat this gate as a replacement for, or superset of, [[004-9-memory-write-gate]]'s quality
  scoring — the two compose, they do not substitute for each other

---

## 6. GitHub Issues

| Issue | Description |
|---|---|
| #6490 | Original MemGhost disclosure |
| #6544 | Initial implementation (write-time provenance, confirm/disclose gates, audit) |
| #6556 | Provenance mislabeling (source_kind derived from trust_level heuristic) |
| #6557 | Missing test coverage for the disclosure-note branch |
| #6558 | Cross-turn deferral TOCTOU bypass |
| #6559 | `audit_all` not honored on interactive path |
| #6569 | Same-tier/cross-tier parallel-dispatch race |
| #6598 | Umbrella fix: closes #6558 + #6569 + reload/compaction residuals |
| #6599 | Umbrella fix: closes #6556 + #6557 + #6559 |

---

## 7. See Also

- [[constitution]] — project principles
- [[004-memory/spec]] — memory system parent index
- [[004-9-memory-write-gate]] — MemReader write quality gate (orthogonal, noise-control)
- [[004-16-shadow-memory-safety]] — MAGE shadow memory (orthogonal, trajectory-level attack defense)
- [[039-background-task-supervisor/spec]] — non-blocking contract this gate's background path follows
- [[001-system-invariants/spec]] — system-wide non-negotiable rules
- [[MOC-specs]] — all specifications
