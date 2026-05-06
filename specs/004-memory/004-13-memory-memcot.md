---
aliases:
  - MemCoT
  - SemanticStateAccumulator
  - Zoom-In Recall
  - Zoom-Out Recall
tags:
  - sdd
  - spec
  - memory
  - retrieval
  - experimental
created: 2026-05-06
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-7-memory-apex-magma]]"
  - "[[012-graph-memory/spec]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: MemCoT — Test-Time Memory Chain-of-Thought

> [!info]
> Training-free multi-view long-term memory (LTM) perception layer with a dual
> short-term memory model. Implemented in PR #3592. Controlled by
> `[memory.memcot] enabled` (default: false).

## Sources

### External
- **MemCoT: Test-Time Memory Scaling via Memory Chain-of-Thought** (arXiv:2604.08216, 2026) —
  Zoom-In/Zoom-Out dual-view retrieval + SemanticStateAccumulator; GPT-4o-mini F1 = 58.03
  on LoCoMo vs ~30 baseline.

### Internal

| File | Contents |
|---|---|
| `crates/zeph-memory/src/memcot/mod.rs` | Module root; `MemCotRecall` entry point |
| `crates/zeph-memory/src/memcot/accumulator.rs` | `SemanticStateAccumulator` — per-turn state tracking |
| `crates/zeph-memory/src/memcot/zoom.rs` | `zoom_in()` and `zoom_out()` retrieval views |
| `crates/zeph-memory/src/memcot/config.rs` | `MemCotConfig` TOML bindings |

---

## 1. Overview

### Problem Statement

Standard semantic recall returns the top-K most similar past memories regardless of
the current reasoning state. Two failure modes:

1. **Evidence fragmentation**: A question like "what was the outcome of the call with Alice
   last Tuesday?" requires locating the specific call record (evidence localization).
   Top-K cosine recall returns similar but different calls, burying the relevant one.
2. **Missing causal context**: Understanding why something happened requires expanding
   from the specific fact to its surrounding causal/temporal neighborhood. Top-K returns
   isolated facts without causal chain.

### Goal

Augment the existing `SemanticMemory::recall` pipeline with two complementary retrieval
views and a per-turn semantic state tracker:

- **Zoom-In**: narrows the query to localize specific evidence within the APEX-MEM resolved
  edge set and the conversation history. Prioritizes precision over coverage.
- **Zoom-Out**: expands the query to causal/contextual neighbors of recalled facts.
  Prioritizes coverage to surface why/how chains.
- **`SemanticStateAccumulator`**: maintains a rolling semantic state across the session —
  a compressed representation of what the agent "knows so far" that biases recall queries.

### Out of Scope

- Modifications to the APEX-MEM write path (MemCoT operates above the edge-resolution layer)
- Training or fine-tuning any model
- Changes to the Qdrant schema or SQLite schema
- Multi-session accumulator persistence across process restarts (accumulator is in-memory only)

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-MC-001 | WHEN `memory.memcot.enabled = false` THE SYSTEM SHALL behave identically to pre-MemCoT recall — no code paths activated, no allocations beyond the disabled-check | must |
| FR-MC-002 | WHEN MemCoT is enabled THE SYSTEM SHALL run Zoom-In and Zoom-Out as two parallel recall passes via `FuturesUnordered`, merging results before injecting into context | must |
| FR-MC-003 | Zoom-In SHALL re-rank the top-K recall results by an evidence-localization score derived from the current user query and the accumulated semantic state | must |
| FR-MC-004 | Zoom-Out SHALL expand each Zoom-In result to its K-nearest Qdrant neighbors and include neighbors not already in the Zoom-In set | must |
| FR-MC-005 | WHEN the token budget for the Zoom-Out expansion exceeds `memcot_budget_tokens` THE SYSTEM SHALL truncate the expansion set (lowest-score items removed first) | must |
| FR-MC-006 | `SemanticStateAccumulator` SHALL be updated at the end of every agent turn with a compressed representation of the assistant's response | must |
| FR-MC-007 | The accumulator update SHALL use a provider specified by `memcot_provider` (or the primary provider if empty); the update MUST be fire-and-forget (not block the turn) | must |
| FR-MC-008 | WHEN the accumulator has no state (first turn) THE SYSTEM SHALL fall back to standard recall for that turn | must |
| FR-MC-009 | WHEN `memcot_provider` returns an error during accumulator update THE SYSTEM SHALL log at `WARN` and retain the previous accumulator state — updates are best-effort | must |
| FR-MC-010 | WHEN the assembler collects context slots THE SYSTEM SHALL inject MemCoT recall results into the `semantic_recall` slot, replacing (not appending to) standard recall when MemCoT is enabled | must |

---

## 3. Component Design

### SemanticStateAccumulator

Maintains a compressed representation of the agent's current understanding.

```rust
pub struct SemanticStateAccumulator {
    /// Compressed state embedding or summary string.
    state: Option<SemanticState>,
    cfg:   MemCotConfig,
}

pub enum SemanticState {
    /// Raw text summary of the agent's current understanding.
    TextSummary(String),
}

impl SemanticStateAccumulator {
    /// Called once per turn after the assistant response is finalized.
    /// Fire-and-forget: spawns a background task via TaskSupervisor.
    pub fn update_async(&mut self, turn_text: &str, provider: &AnyProvider);

    /// Snapshot used by Zoom-In to bias the recall query.
    pub fn snapshot(&self) -> Option<&SemanticState>;
}
```

The accumulator update prompt compresses the current turn text + prior state into a
short (≤ 256 tokens) semantic state summary. This summary biases the Zoom-In
re-ranking step.

### Zoom-In Retrieval

Zoom-In takes the standard top-K Qdrant recall results and re-ranks them:

```
score(m) = α × cosine(query, m) + β × cosine(accumulator_state, m)
```

where `α + β = 1.0` (configurable; defaults `α = 0.7`, `β = 0.3`).

The result is the top-N re-ranked messages. This biases recall toward memories
consistent with the current semantic state, not just the raw query.

### Zoom-Out Expansion

Zoom-Out expands each Zoom-In result `m_i`:

1. Find the K-nearest Qdrant neighbors of `m_i` (configurable `zoom_out_k`, default 3)
2. Include neighbors not already in the Zoom-In set
3. Score neighbors as `cosine(query, neighbor)` (no state bias — pure topical expansion)
4. Merge: Zoom-In results ranked first, Zoom-Out results appended, total token budget capped

---

## 4. Config

```toml
[memory.memcot]
enabled           = false            # default off; opt-in
memcot_provider   = ""               # [[llm.providers]] name for accumulator updates; empty = primary
zoom_in_alpha     = 0.7              # weight for query similarity in re-ranking
zoom_in_beta      = 0.3              # weight for semantic state similarity in re-ranking
zoom_out_k        = 3                # neighbors to expand per Zoom-In result
memcot_budget_tokens = 512           # token cap on the full MemCoT recall slot
```

---

## 5. Key Invariants

- **Disabled = zero overhead.** When `enabled = false`, `MemCotRecall` is never constructed and no allocations are made for accumulator or zoom passes.
- **Accumulator updates are fire-and-forget.** They MUST NOT block the agent turn response path.
- **Zoom-In and Zoom-Out run in parallel.** Both passes use `FuturesUnordered`; the merge step waits for both.
- **MemCoT replaces, not appends.** When enabled, MemCoT results replace the standard semantic recall slot; the two paths are mutually exclusive per turn.
- **Accumulator failure is non-fatal.** An error during the accumulator update retains the previous state; the system degrades to biasing recall with the prior state rather than the current turn.
- **NEVER inject Zoom-Out neighbors that exceed the token budget.** Truncation is mandatory when the expanded set would overflow `memcot_budget_tokens`.
- **NEVER persist the accumulator state across process restarts.** It is session-scoped and in-memory only.

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| First turn (accumulator empty) | Fall back to standard recall; no Zoom-In re-ranking bias applied |
| `memcot_provider` unavailable at startup | Log `WARN`; MemCoT enabled but accumulator updates disabled; Zoom-In runs with `β = 0` (query-only) |
| Zoom-Out expansion returns 0 neighbors for all Zoom-In results | Return Zoom-In results only; no expansion; log at `DEBUG` |
| Zoom-In returns 0 results (empty recall) | Skip Zoom-Out; return empty slot; standard recall fallback is NOT triggered (MemCoT slot stays `None`) |
| Budget exhausted after Zoom-In alone | Truncate Zoom-In results to budget; skip Zoom-Out entirely |
| Qdrant unavailable mid-recall | Propagate error; ContextAssembler treats slot as `None` (graceful degradation per FR-005 in 021-zeph-context) |

---

## 7. Acceptance Criteria

- `cargo nextest run -p zeph-memory -E 'test(memcot)'` passes
- A session with `enabled = true` and a multi-turn conversation: accumulator state is non-empty after turn 1; Zoom-In scores differ from raw cosine ordering; Zoom-Out set contains at least one neighbor not in Zoom-In for typical queries
- A session with `enabled = false`: no `MemCotRecall` allocations appear in the trace; `semantic_recall` slot is filled by the standard path
- Budget overflow test: Zoom-Out expansion that would exceed `memcot_budget_tokens` is truncated; total slot token count ≤ budget
- Accumulator update error: provider returns `Err`; accumulator retains previous state; next turn's Zoom-In uses the prior state without panicking

---

## 8. See Also

- [[004-memory/spec]] — parent memory spec
- [[004-7-memory-apex-magma]] — APEX-MEM (MemCoT operates above the edge-resolution layer)
- [[012-graph-memory/spec]] — SYNAPSE spreading activation (Zoom-Out neighbor traversal is complementary)
- [[021-zeph-context/spec]] — `ContextAssembler` slot model
- [[024-multi-model-design/spec]] — `memcot_provider` tier guidance
- [[MOC-specs]] — all specifications
