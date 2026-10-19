---
aliases:
  - Speculation Engine
  - Speculative Tool Execution
  - PASTE Skill Activation
  - Decoding Speculation
tags:
  - sdd
  - spec
  - core
  - performance
  - tools
  - contract
created: 2026-05-06
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[006-tools/spec]]"
  - "[[003-llm-providers/spec]]"
---

# Spec: SpeculationEngine — Speculative Tool Execution

> [!info]
> Reduces tool-dispatch latency by pre-executing tool calls before the LLM finishes
> generating them. Two complementary strategies share one bounded cache and one sweeper.
> Landed across PRs #3636, #3640, #3647, #3651, #3654.

## Sources

### Internal

| File | Contents |
|---|---|
| `crates/zeph-core/src/agent/speculative/mod.rs` | `SpeculationEngine` — main entry point, `try_dispatch`, `has_match`, `commit_within`, `discard`, `cancel_pending`, `end_turn` |
| `crates/zeph-core/src/agent/speculative/cache.rs` | `SpeculativeCache`, `SpeculativeHandle`, TTL enforcement |
| `crates/zeph-core/src/agent/speculative/partial_json.rs` | `PartialJsonParser` — streaming SSE JSON accumulation |
| `crates/zeph-core/src/agent/speculative/stream_drainer.rs` | SSE stream integration — fires `try_dispatch` on partial tool completions |
| `crates/zeph-core/src/agent/speculative/paste.rs` | PASTE path — `try_dispatch` at skill activation boundary |
| `crates/zeph-core/src/agent/speculative/prediction.rs` | `Prediction`, `PredictionSource` |
| `crates/zeph-config/src/tools.rs` | `SpeculativeConfig`, `SpeculationMode` |

---

## 1. Overview

### Problem Statement

Tool dispatch is synchronous with the LLM generation stream: the agent waits for the
full tool-call JSON to arrive, then executes. For deterministic or near-deterministic
tools (read-only filesystem reads, shell commands with known args), this is wasted
latency — the arguments are often known from partial SSE output, or predictable from
skill activation patterns.

### Goal

Pre-execute tool calls speculatively while the LLM is still generating, so that when
the confirmed call arrives the result is already cached. On a commit hit, the confirmed
call returns the pre-computed result immediately.

### Out of Scope

- Speculative writes to shared mutable state that cannot be rolled back
- Any form of dry-run execution (confirmation is a pure policy query, not a sandbox run)
- Speculation on tools from untrusted skills
- Speculation in channels where trust cannot be established

---

## 2. Speculation Modes

`SpeculationMode` is an enum in `zeph-config`:

| Variant | Trigger |
|---|---|
| `Off` (default) | No speculation; engine is a no-op |
| `Decoding` | SSE stream — `PartialJsonParser` accumulates tokens; `try_dispatch` fires when all required JSON fields are complete |
| `Pattern` | PASTE — `try_dispatch` fires at skill-activation time using historical invocation sequences from SQLite |

Both modes share the same `SpeculationEngine::try_dispatch` / `commit_within` API and the same `SpeculativeCache`.

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-SE-001 | WHEN `mode = off` THE SYSTEM SHALL perform no speculative work — no allocations, no background tasks beyond the sweeper | must |
| FR-SE-002 | WHEN `mode = decoding` THE SYSTEM SHALL parse partial SSE tool-call JSON via `PartialJsonParser::push()` and call `engine.try_dispatch()` as soon as all required fields (`tool_id`, `params`) are parseable | must |
| FR-SE-003 | WHEN `mode = pattern` (PASTE) THE SYSTEM SHALL call `engine.try_dispatch()` at skill activation with a `PredictionSource::SkillActivation` prediction | must |
| FR-SE-004 | WHEN `try_dispatch` is called AND `trust_level != Trusted` THE SYSTEM SHALL skip dispatch and return `false` | must |
| FR-SE-005 | WHEN `try_dispatch` is called AND `executor.requires_confirmation_erased(&call)` returns `true` THE SYSTEM SHALL skip dispatch, increment `skipped_confirmation` metric, and return `false` — no tool is executed | must |
| FR-SE-006 | WHEN `try_dispatch` is called AND all gates pass THE SYSTEM SHALL execute the tool in a background task via `TaskSupervisor::spawn_oneshot`, store the handle in `SpeculativeCache`, and return `true` | must |
| FR-SE-007 | WHEN `commit_within(call, limit)` is called THE SYSTEM SHALL look up the cache by `(tool_id, args_hash, context_hash)` and return `CommitOutcome::Committed` with the handle result if it finishes within `limit`, `TimedOut` if it does not (the task is cancelled and aborted via `BlockingHandle::abort_handle`, counted in `cancelled` and `wasted_ms`, never in `committed`), or `Miss` when no handle exists | must |
| FR-SE-011 | WHEN the normal path resolves a call without consuming its handle (rejected by a gate, served from the result cache, blocked by a hook) THE SYSTEM SHALL call `discard(call)`, which cancels and aborts the handle and counts it as `cancelled` | must |
| FR-SE-012 | WHEN `[tools] enabled = false` THE SYSTEM SHALL NOT dispatch speculatively; WHEN the session tool-call quota is spent THE SYSTEM SHALL NOT start speculation. The quota reason (`DisabledReason::QuotaExhausted`) exists only on the speculative gates (`speculative_dispatch_gate`, `paste_dispatch_gate`); the normal `dispatch_gate` never reads quota state because the quota is charged per batch before any tier runs | must |
| FR-SE-013 | WHEN a speculative stream fails THE SYSTEM SHALL call `cancel_pending()` (cancel and abort all handles, metrics not reset), then retry once without streaming; the same applies when the drain is cancelled or times out | must |
| FR-SE-008 | WHEN `end_turn` is called THE SYSTEM SHALL cancel all in-flight handles via `CancellationToken` and reset `SpeculativeMetrics` | must |
| FR-SE-009 | WHEN the sweeper interval fires (every 5 s) THE SYSTEM SHALL evict handles whose `ttl_deadline` has passed | must |
| FR-SE-010 | WHEN `SpeculationEngine` is dropped THE SYSTEM SHALL abort the sweeper task and cancel all cache handles | must |
| FR-SE-011 | WHEN a `ToolStartEvent` is emitted for a speculatively dispatched call THE SYSTEM SHALL set `speculative: true` on the event | must |

---

## 4. Safety Invariants

1. **Trust gate first.** `try_dispatch` returns `false` immediately when `trust_level != Trusted` — before any tool metadata is read.
2. **Confirmation gate is a pure policy query.** `executor.requires_confirmation_erased(&call)` is a metadata read; it does NOT execute the tool. No double side-effects.
3. **Speculative dispatch uses `execute_tool_call`, never `execute_tool_call_confirmed`.** There is no fenced-block bypass.
4. **All handles are cancelled at turn boundary.** `end_turn()` calls `cache.cancel_all()`. No handle outlives its turn.
5. **Sweeper shares the same cache inner `Arc`.** The sweeper operates on the live handle set, not a stale snapshot (C2 invariant).
6. **Cache is bounded.** `max_in_flight` (config, default 8) is enforced by `SpeculativeCache::insert`; when full, the oldest handle is evicted and its task aborted.
7. **TTL is per-handle.** Default `ttl_seconds = 30`. The sweeper enforces TTL independently of turn boundaries.

---

## 5. Config Schema

```toml
[tools.speculation]
mode                = "off"     # "off" | "decoding" | "pattern"
max_in_flight       = 8         # max concurrent speculative handles
ttl_seconds         = 30        # per-handle TTL (sweeper enforced)
confidence_threshold = 0.7      # minimum Prediction.confidence to dispatch
```

---

## 6. Metrics (`SpeculativeMetrics`)

Collected per turn and reset by `end_turn()`:

| Field | Description |
|---|---|
| `committed` | Handles that matched and returned a cached result |
| `cancelled` | Handles cancelled (mismatch, TTL, turn end) |
| `evicted_oldest` | Handles evicted because `max_in_flight` was saturated |
| `skipped_confirmation` | Calls skipped because `requires_confirmation` returned `true` |
| `wasted_ms` | Total wall-clock ms spent in cancelled speculative work |

---

## 7. Wiring

### SSE Decoding Path (`mode = decoding`)

```
LLM SSE stream
    │
    ▼
stream_drainer::drain_stream()
    │  PartialJsonParser::push(token)
    │  → if complete_tool_args → engine.try_dispatch(prediction, trust_level)
    │
    ▼
normal tool dispatch → all gates (pre-hooks, quota, pre-exec, turn set, requires, cache, rate limit, before_tool)
    │  rejected → engine.discard(call)
    ▼
single commit point → engine.commit_within(call, COMMIT_LIMIT)
    │  Committed → cached result
    │  TimedOut / Miss → executor.execute_tool_call(call)
```

### PASTE Path (`mode = pattern`)

```
skill activation boundary (assembly.rs:1284)
    │
    ▼
observe_paste_transition(native.rs:3576)
    │  predict likely next tool calls from SQLite
    │  → engine.try_dispatch(prediction, trust_level)
    │
    ▼
normal tool dispatch → same single commit point (`commit_within`)
```

---

## 8. Key Invariants

- NEVER dispatch speculatively when `trust_level != Trusted`
- NEVER use `execute_tool_call_confirmed` (fenced-block path) for speculative dispatch
- NEVER retain in-flight handles beyond turn boundary — `end_turn()` is mandatory
- NEVER reuse a stale TTL-expired handle — sweeper evicts them within 5 s
- NEVER share a sweeper operating on an empty clone of the cache (the sweeper arc is the same as the engine's cache arc)
- NEVER set `speculative: false` on `ToolStartEvent` when a speculative result existed when dispatch started (`has_match` peek; telemetry only, the commit itself happens later)
- NEVER commit a speculative handle before every normal-path gate has passed; commit happens at exactly one point, after the gates
- NEVER let a speculative task outlive its commit attempt: a timed-out commit cancels and aborts it (note: abort does not stop work already running on a `spawn_blocking` thread)
- Execution before commit is bounded by `is_tool_speculatable`, the no-confirmation check and the dispatch gate; the PASTE gate enforces `requires` only because the turn's tool set does not exist yet at skill activation, turn-set membership is enforced at commit

---

## 9. Acceptance Criteria

- `cargo nextest run -p zeph-core -E 'test(speculation)'` passes
- A `mode = decoding` session: SSE tool-call tokens accumulate in `PartialJsonParser`; `try_dispatch` fires before the LLM finishes; `commit_within` returns the cached result (committed counter > 0)
- A `mode = pattern` session: PASTE activation calls `try_dispatch`; committed counter reflects hits
- An untrusted skill call: `skipped_confirmation` or dispatch skipped entirely; no speculative task spawned
- After `end_turn()`: `cache.is_empty()` is `true`; metrics reset to zero
- `SpeculationEngine` drop: sweeper task aborted without panic (both supervised and raw paths)
- `ToolStartEvent { speculative: true }` appears in the event stream when a speculative result existed at dispatch start
- A call rejected by a gate leaves `committed = 0` and `cancelled = 1`; a commit that exceeds its limit leaves `committed = 0`, `cancelled = 1` and the task never completes
- `mode = off`: no background tasks spawned; `is_active()` returns `false`
