---
aliases:
  - TurnContext Spec
tags:
  - sdd
  - agent
  - refactor
created: 2026-04-27
status: permanent
related:
  - "[[049-agent-decomposition]]"
  - "[[002-agent-loop]]"
---

# TurnContext Value Type

**Issue:** #3510 — P2-prereq-3 for Agent god-object decomposition (#3498)

## Purpose

`TurnContext` is a `Send + 'static` owned value type defined in `zeph-context` that carries
per-turn invariants needed by every phase of the agent loop (`loop`, `compose`, `persist`).
It enables the Phase 2 crate extraction (#3498) to accept turn-scoped state without
passing `&mut Agent<C>` across crate boundaries.

## Fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | `TurnId` | Monotonically increasing turn identifier within the conversation |
| `cancel_token` | `CancellationToken` | Per-turn cancellation signal; created fresh each turn |
| `timeouts` | `TimeoutConfig` | Timeout policy snapshot; stable for the duration of the turn |

## Key Invariants

- `TurnContext` MUST be `Send + 'static` at all times — no borrows, no non-Send fields
- `TurnId` is defined in `zeph-context`, NOT `zeph-core`, to avoid a forbidden inverted dependency
- `cancel_token` is created in `Agent::begin_turn` and cloned into `runtime.lifecycle.cancel_token` as a transitional mirror until Phase 2 consolidates the two (TODO #3498)
- `timeouts` is snapshotted by value (`TimeoutConfig: Copy`) — never a reference to live config
- NEVER wrap `TurnContext` in `Arc<Mutex<...>>` — it is a value type for a reason

## Ownership Model

```
Agent::begin_turn()
  → constructs TurnContext (id + cancel_token + timeouts)
  → wraps in Turn { context, input, metrics }

Turn lives on the call stack of process_user_message.
TurnContext is accessible via turn.context.
In Phase 2: TurnContext will be passed by value (clone) to sub-services in other crates.
```

## Phase 2 Extensions (deferred to #3498)

The following fields are intentionally NOT in this PR:
- `tool_allowlist: Option<Vec<String>>` (field scaffolded as `None`; Phase 2 populates from channel config)
- Provider snapshots — requires Services decomposition to complete first
- `MessageState` snapshot — requires persistence extraction crate
