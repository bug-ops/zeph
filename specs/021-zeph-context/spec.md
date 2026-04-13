---
aliases:
  - Context Budget
  - Context Manager
  - Context Assembler
  - Context Crate
tags:
  - sdd
  - spec
  - core
  - context
  - compaction
created: 2026-04-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[004-memory/spec]]"
---

# Spec: Context Crate (`zeph-context`)

> [!info]
> Token budget calculation, context lifecycle state machine, and stateless context assembler
> for the Zeph agent. Extracted from `zeph-core` to eliminate monolith growth and improve
> compile-time isolation. Has no dependency on `zeph-core`.

## 1. Overview

### Problem Statement

Context management grew into a large, intertwined subsystem inside `zeph-core`. Compaction
logic, budget arithmetic, and parallel fetch coordination mixed with agent loop concerns,
creating long rebuild times and making the subsystem difficult to test in isolation.

### Goal

Provide a self-contained crate (`zeph-context`) that owns the **stateless and data-only**
parts of context management: budget arithmetic, compaction state machine, parallel context
assembly, and associated helpers. `zeph-core` depends on this crate but not vice versa.

### Out of Scope

- Agent loop control flow (owned by `zeph-core`)
- LLM calls for summarization (callers in `zeph-core` pass results in)
- Channel communication during assembly (all `send_status` calls stay in `zeph-core`)
- Persistence (owned by `zeph-memory`)

---

## 2. User Stories

### US-001: Budget Allocation

AS A `zeph-core` agent loop
I WANT to compute a per-slot token budget for the current turn
SO THAT each context source (summaries, graph facts, semantic recall, code context, etc.)
receives a fair share and the response reserve is always protected.

**Acceptance criteria:**

```
GIVEN the model's context window size and current prompt usage
WHEN ContextBudget::allocate() is called
THEN a BudgetAllocation is returned with non-negative per-slot values
AND the sum of all slots plus response_reserve does not exceed the total window
AND slots are zero when disabled or budget is exhausted
```

### US-002: Compaction State Machine

AS A `zeph-core` agent loop
I WANT a strict state machine to track compaction lifecycle
SO THAT invalid states (e.g., compaction warning without exhaustion, double-compact per turn)
are structurally unrepresentable.

**Acceptance criteria:**

```
GIVEN CompactionState::Ready
WHEN hard compaction succeeds
THEN state transitions to CompactedThisTurn { cooldown }

GIVEN CompactionState::CompactedThisTurn { cooldown: 0 }
WHEN advance_turn() is called
THEN state transitions to Ready

GIVEN CompactionState::Exhausted { warned: false }
WHEN the warning is dispatched
THEN state transitions to Exhausted { warned: true } and no further transitions occur
```

### US-003: Parallel Context Assembly

AS A `zeph-core` agent loop
I WANT all context sources fetched concurrently in a single pass
SO THAT context assembly latency is bounded by the slowest single source, not their sum.

**Acceptance criteria:**

```
GIVEN a ContextAssemblyInput with multiple enabled sources
WHEN ContextAssembler::gather() is called
THEN all source futures are driven via FuturesUnordered concurrently
AND the result is a PreparedContext with each slot as Option (None = disabled / empty)
AND no Agent fields are mutated inside gather()
AND no channel messages are sent inside gather()
```

### US-004: Microcompact Detection

AS A `zeph-core` agent loop
I WANT to detect turns dominated by low-value tool results
SO THAT microcompact eviction can target the right messages without reading full history.

**Acceptance criteria:**

```
GIVEN a recent turn's tool results
WHEN microcompact::is_low_value_result() is called
THEN it returns true for outputs matching known low-signal patterns
AND it returns false for results with meaningful content
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `ContextBudget::allocate()` is called THEN the system SHALL return a `BudgetAllocation` where the sum of all slots plus `response_reserve` is less than or equal to the total context window | must |
| FR-002 | WHEN any context slot exceeds its allocated budget THEN the system SHALL truncate or omit that slot rather than overflow into adjacent slots | must |
| FR-003 | WHEN `CompactionState` transitions are applied THEN the system SHALL enforce the documented state machine and reject invalid transitions | must |
| FR-004 | WHEN `ContextAssembler::gather()` is called THEN the system SHALL fan out all source futures concurrently using `FuturesUnordered` and collect all results before returning | must |
| FR-005 | WHEN a context source returns an error THEN the system SHALL log the error at `WARN` level and treat that slot as `None` (graceful degradation) | must |
| FR-006 | WHEN `ContextManager` is queried for compaction eligibility THEN it SHALL check both the compaction state and the turn-boundary cooldown before returning a recommendation | must |
| FR-007 | WHEN summarization prompt helpers are called THEN the system SHALL produce deterministic prompt strings given the same inputs | should |
| FR-008 | WHEN `compression_feedback` detects context loss THEN it SHALL classify the failure into a `CompressionFailureClass` for downstream handling | should |
| FR-009 | WHEN slot helpers compute a trimmed slice THEN the system SHALL return only the tokens that fit within the allocated budget and not mutate the source | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | `ContextBudget::allocate()` must complete in < 1 µs (pure arithmetic, no I/O) |
| NFR-002 | Performance | `ContextAssembler::gather()` latency must equal max(individual source latency) not sum(all latencies) |
| NFR-003 | Correctness | `CompactionState` must be `Copy` — transitions must be side-effect-free value operations |
| NFR-004 | Isolation | `zeph-context` must not depend on `zeph-core`; the dependency DAG is strictly downward |
| NFR-005 | Testability | Budget and state machine logic must be testable with unit tests only (no external services) |
| NFR-006 | Safety | No `unsafe` code; `unsafe_code = "deny"` workspace lint applies |

---

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `ContextBudget` | Token budget for a single agent session | `max_tokens`, per-slot ratios, `response_reserve` |
| `BudgetAllocation` | Result of one budget split | Per-slot token counts: `summaries`, `semantic_recall`, `cross_session`, `code_context`, `graph_facts`, `recent_history`, `response_reserve`, `session_digest` |
| `ContextManager` | Compaction lifecycle tracker | `CompactionState`, `turns_since_last_hard_compaction`, routing config |
| `CompactionState` | State machine enum | Variants: `Ready`, `CompactedThisTurn { cooldown }`, `Cooling { turns_remaining }`, `Exhausted { warned }` |
| `ContextAssembler` | Stateless parallel fetch coordinator | Takes `ContextAssemblyInput`, returns `PreparedContext` |
| `ContextAssemblyInput` | Borrowed view of all assembly inputs | References to memory stores, index, config, token counter |
| `PreparedContext` | Result of one assembly pass | `graph_facts`, `doc_rag`, `semantic_recall`, `cross_session`, `summaries`, `corrections` — all `Option<Message>` |
| `ContextSlot` | Single fetched context source result | Content string, token count, source label |
| `CompressionFailureClass` | Classification of a compaction failure | Enum: `ContextLoss`, `OversizedInput`, `SummarizationFailure`, `NothingToCompact` |

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Total budget < response_reserve | All source slots are zero; only response_reserve is allocated |
| Single source fetch panics | Other sources continue; panic is caught at the assembler boundary |
| Source returns empty content | Slot is set to `None`; budget is not charged |
| `CompactionState::Exhausted` reached | No further compaction attempts; one user-facing warning dispatched |
| Cooldown turns = 0 | State immediately advances to `Ready` on `advance_turn()` |
| Budget arithmetic overflow | Saturating arithmetic used throughout; no panic |

---

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Unit test coverage for `CompactionState` transitions | All documented transitions have a test |
| SC-002 | `ContextAssembler::gather()` in tests | All futures are driven concurrently (verified via timing) |
| SC-003 | Compile-time isolation | `cargo check -p zeph-context` completes without `zeph-core` in the dependency graph |
| SC-004 | Budget arithmetic | `BudgetAllocation` slot sum ≤ max_tokens in all property tests |

---

## 8. Agent Boundaries

### Always (without asking)
- Run `cargo nextest run -p zeph-context` after changes
- Follow stateless-only convention: no agent fields mutated inside `gather()`

### Ask First
- Adding new context slots to `BudgetAllocation` (affects all callers in `zeph-core`)
- Changing `CompactionState` transitions (must update the documented transition map)
- Adding dependencies to `zeph-context`'s `Cargo.toml`

### Never
- Add a dependency on `zeph-core`
- Perform channel I/O or logging to users inside `ContextAssembler::gather()`
- Use `unsafe` blocks

---

## 9. Open Questions

None.

---

## 10. See Also

- [[constitution]] — project principles
- [[001-system-invariants/spec]] — cross-cutting invariants
- [[002-agent-loop/spec]] — agent loop that consumes this crate
- [[004-memory/spec]] — memory stores queried by `ContextAssembler`
- [[MOC-specs]] — all specifications
