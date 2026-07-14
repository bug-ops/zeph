---
aliases:
  - MemGuard
  - Type-Aware Retrieval Composition
  - Functional Memory Type Gating
tags:
  - sdd
  - spec
  - memory
  - context
  - research
created: 2026-07-15
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[021-zeph-context/spec]]"
  - "[[024-multi-model-design/spec]]"
  - "[[043-zeph-common/spec]]"
---

# Spec: MemGuard — Type-Aware Retrieval Composition

> [!info]
> Backfilled after-the-fact per this project's `/sdd` convention (CLAUDE.md: "If a spec is
> missing... create or update it in `/specs/` using `/sdd` before writing code"). The feature
> shipped in commit `2e8d0969` (#6226, closing research issue #6086) with only an ephemeral
> planning doc at `.local/specs/064-memguard-type-aware-memory-retrieval/spec.md` (a session-local
> working file, not the permanent `/specs/` index) — that ephemeral spec's rustdoc citations
> ("spec 064 §4", "spec 064 §3 Q3") shipped verbatim into the production code comments in
> `crates/zeph-common/src/memory.rs`, `crates/zeph-config/src/memory/retrieval.rs`, and
> `crates/zeph-agent-context/src/type_aware_compose.rs`. **This is a naming collision**: `064` in
> the permanent `/specs/` numbering is already assigned to
> [[064-durable-execution/spec|Durable Execution]] — a reader following those in-code citations
> into `/specs/064-durable-execution/spec.md` would land on the wrong subsystem entirely. This
> document is filed as `004-16` (memory sub-spec numbering, following the `004-N` convention
> already used for 004-1 through 004-15) specifically to avoid colliding with the permanent `064`
> slot. The in-code comment citations are a known, minor documentation-staleness artifact — not
> corrected here since this task is scoped to `/specs/`, not `crates/`; flagged for a future
> doc-only follow-up.

## Sources

### External
- **MemGuard: Preventing Memory Contamination in Long-Term Memory-Augmented Large Language
  Models** (arXiv:2605.28009) — memory contamination occurs when distinct functional memory
  categories (user facts, episodic events, behavioral rules) collapse into one shared retrieval
  pool; the paper's fix is type-at-creation-time isolation plus retrieval that selectively
  composes only the functionally relevant type(s), reporting up to 28.27% memory-reliability
  improvement while retrieving up to 5.8x fewer memory tokens.

### Internal
| File | Contents |
|---|---|
| `crates/zeph-common/src/memory.rs` | `FunctionalType` enum (`Episodic`, `UserFact`, `BehavioralRule`, `ReasoningStrategy`, `CrossSessionSummary`, `GraphFact`), `#[non_exhaustive]`, strict `FromStr` (unknown string is a hard error, never a silent "all types" fallback) |
| `crates/zeph-config/src/memory/retrieval.rs` | `TypeAwareComposeConfig { enabled, default_compose_types, intent_scoped }` |
| `crates/zeph-config/src/memory/root.rs` | `MemoryConfig.type_aware_compose: TypeAwareComposeConfig` |
| `crates/zeph-agent-context/src/type_aware_compose.rs` | `resolve_active_functional_types(config, query) -> Vec<FunctionalType>` — pure active-set resolution; static `IntentClass -> FunctionalType[]` widening table |
| `crates/zeph-context/src/assembler.rs` | `schedule_context_fetchers` — gates five of the six `ContextAssembler` fetchers on the active set |
| `crates/zeph-memory/src/semantic/recall.rs` | `recall_with_category` — pre-existing category filter this feature finally wires a caller onto |
| `crates/zeph-memory/src/tiered_retrieval.rs` | `MemFlow` tiered pipeline; previously passed `category: None` unconditionally |
| `config/default.toml` | `[memory.type_aware_compose]` documented-commented-out block |

---

## 1. Overview

### Problem Statement

`zeph-memory` already has strong *storage*-side functional isolation — separate SQLite tables
per memory function (`persona_memory`, `consolidated_facts`, `user_corrections`,
`learned_preferences`, `trajectory_memory`, `graph_episodes`/`graph_entities`/`graph_edges`) — but
the *retrieval* side has no equivalent. `recall_with_category` (`semantic/recall.rs`) existed as
dead code from the production path's perspective: `MemFlow`'s default recall passed
`category: None` unconditionally, so every turn composed all functional memory types into one
undifferentiated pool regardless of the actual retrieval need, per the code-grounded audit filed
as issue #6086.

### Goal

Context assembly can, when opted in, compose only the functionally relevant memory type(s) for a
turn instead of always fetching all six sources — reducing irrelevant-context token usage and the
contamination risk MemGuard documents, consistent with the paper's approach and Zeph's own
already-partitioned SQL-table structure.

### Out of Scope

- No new storage tier, no new Qdrant collection, no write-path change — retrieval-only,
  fetch-time gate.
- `BehavioralRule` (past-correction recall, `fetch_corrections`) is never gated — it stays
  unconditionally composed as a safety-critical invariant regardless of the active set (§4).
- No LLM-based intent classification — `intent_scoped` reuses the existing no-I/O
  `HeuristicRouter`, adding zero new LLM calls.
- Per-model/per-provider composition tuning — out of scope; the active set is global per turn.

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `memory.type_aware_compose.enabled = false` (default) THE SYSTEM SHALL compose every memory source exactly as it did before this feature — byte-for-byte no-op | must |
| FR-002 | WHEN `enabled = true` AND `default_compose_types` is non-empty THE SYSTEM SHALL gate `schedule_context_fetchers` to compose only the functional types in the active set | must |
| FR-003 | WHEN `enabled = true` AND `default_compose_types` is empty AND `intent_scoped = false` THE SYSTEM SHALL treat the active set as "all types" — identical to `enabled = false` | must |
| FR-004 | WHEN `intent_scoped = true` THE SYSTEM SHALL classify the query via the existing no-LLM `HeuristicRouter`/`IntentClass` and widen (never narrow) the active set per a static `IntentClass -> FunctionalType[]` table | must |
| FR-005 | WHEN a config value in `default_compose_types` does not match a known `FunctionalType` variant THE SYSTEM SHALL fail config load with a hard error — never silently widen to "all types" | must |
| FR-006 | WHEN `fetch_corrections` (`BehavioralRule`) would run THE SYSTEM SHALL always schedule it regardless of the active set | must |
| FR-007 | WHEN a future `FunctionalType` variant is added to the `#[non_exhaustive]` enum THE SYSTEM SHALL treat it as always-composed until a fetcher explicitly gates on it — never silently dropped | should |

---

## 3. Architecture

### 3.1 Data Model

```rust
#[non_exhaustive]
pub enum FunctionalType {
    Episodic,            // fetch_semantic_recall -> zeph_conversations
    UserFact,             // fetch_persona_facts -> SQL persona_memory
    BehavioralRule,       // fetch_corrections -> zeph_corrections (always-on, never gated)
    ReasoningStrategy,    // fetch_reasoning_strategies -> reasoning_strategies
    CrossSessionSummary,  // fetch_summaries/fetch_cross_session -> zeph_session_summaries
    GraphFact,            // fetch_graph_facts -> zeph_graph_entities
}
```

`FunctionalType` lives in `zeph-common` (not `zeph-memory`) because `zeph-context` — the crate
whose `schedule_context_fetchers` gates on this type — deliberately has no `zeph-memory`
dependency (issue #3665). `zeph-memory` re-exports it at its crate root for taxonomy
discoverability. This is orthogonal to `CompressionLevel`/`MemoryTier` (a storage-tier axis) and
`MemoryRoute` (a routing-backend axis) — a `zeph_conversations` vector is simultaneously
`Episodic`-tier and the `Episodic` functional type.

### 3.2 Config Schema

```toml
[memory.type_aware_compose]
enabled = false            # off by default (#6086)
default_compose_types = [] # empty = all types; strict parse, unknown string is a hard error
intent_scoped = false      # widen active set per classified intent; reuses HeuristicRouter, no new LLM call
```

### 3.3 Active-Set Resolution

```
resolve_active_functional_types(config, query) -> Vec<FunctionalType>
        │
        ├── !config.enabled ────────────────────────────> []  (no gating — compose all)
        │
        ├── default_compose_types.is_empty() && !intent_scoped -> []  (no gating — compose all)
        │
        └── active = default_compose_types.clone()
                 │
                 └── intent_scoped? -> classify(query) via HeuristicRouter -> IntentClass
                                     -> widen `active` per static table (dedup, never narrow)
```

Static `IntentClass -> FunctionalType[]` widening table (v1):

| `IntentClass` | Widens active set with |
|---|---|
| `ProfileLookup` | `UserFact` |
| `TargetedRetrieval` | `Episodic`, `UserFact`, `CrossSessionSummary`, `GraphFact` |
| `DeepReasoning` | `Episodic`, `ReasoningStrategy`, `CrossSessionSummary`, `GraphFact` |
| any other (future, `#[non_exhaustive]`) | `[]` (conservative — no accidental over-composition) |

`schedule_context_fetchers` (`zeph-context/src/assembler.rs`) treats an empty resolved `Vec` as
"compose everything" — the same code path as today, before this feature existed.

---

## 4. Key Invariants

### Always (without asking)

- `enabled = false` (default) reproduces the exact current unfiltered composition,
  byte-for-byte (FR-001).
- `fetch_corrections` (`BehavioralRule`) is scheduled unconditionally, regardless of the active
  set — safety-critical past-correction recall is never gated out (FR-006).
- `resolve_active_functional_types` is pure — no I/O, no LLM call, no randomness; `intent_scoped`
  classification reuses the existing synchronous `HeuristicRouter` (FR-004).
- An unknown/typo'd string in `default_compose_types` is a hard config-load error, never a
  silent widen-to-all fallback (FR-005, critic finding S4 from the originating design review).
- Widening via the `IntentClass` table only adds types to the already-resolved default set — it
  never narrows or replaces it.

### Ask First

- Adding a per-model or per-provider variant of the active-set resolution.
- Extending `intent_scoped` widening to use an LLM classifier instead of `HeuristicRouter` — the
  current design's "no new LLM call" guarantee is a deliberate multi-model-design trade-off
  ([[024-multi-model-design/spec]]).

### Never

- **NEVER** gate `fetch_corrections`/`BehavioralRule` behind the active set — it is always
  composed (FR-006).
- **NEVER** let an unrecognised `default_compose_types` string silently fall back to "all
  types" — fail config load instead (FR-005).
- **NEVER** narrow the active set via intent-scoped widening — the table only adds types.

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `enabled = false` | All memory types composed, identical to pre-feature behavior (FR-001) |
| `enabled = true`, `default_compose_types = []`, `intent_scoped = false` | Treated as "all types" — same as disabled (FR-003) |
| `enabled = true`, `default_compose_types = ["user_fact"]` | Only `UserFact` composed; `BehavioralRule` still always composed alongside it |
| `intent_scoped = true`, query classifies as `DeepReasoning` | Active set widened with `Episodic`/`ReasoningStrategy`/`CrossSessionSummary`/`GraphFact`, deduplicated against any overlapping default types |
| `default_compose_types = ["user_facts"]` (typo, trailing `s`) | Config load fails hard — never silently treated as unknown-so-compose-all |
| A future `FunctionalType` variant is added upstream but no fetcher gates on it yet | Always composed — new variants are opt-in-to-gate, not opt-in-to-compose |

---

## 6. Success Criteria

- [x] `enabled = false` byte-for-byte no-op verified (unit test: `enabled_with_empty_default_and_no_intent_scoping_resolves_to_empty_set`)
- [x] `FunctionalType::from_str` round-trips every variant; rejects unknown/typo strings (`functional_type_from_str_rejects_unknown_string`)
- [x] serde round-trip for every `FunctionalType` variant; rejects unknown JSON variant
- [x] Intent-scoped widening deduplicates against the default set (`intent_scoped_widens_default_set_without_duplicates`)
- [x] `BehavioralRule` never appears in any `IntentClass` widening table entry (`intent_functional_types_never_include_behavioral_rule`)
- [x] `cargo +nightly fmt --check`, `cargo clippy --profile ci ... -D warnings`, `cargo nextest run ...` pass (landed in #6226/PR closing #6086)

---

## 7. Relationship to Existing Specs

| This spec | Existing spec | Relationship |
|-----------|---------------|---------------|
| `FunctionalType`, active-set gating | [[004-memory/spec]] | Retrieval-only extension; no change to the storage-side per-function SQL table isolation already documented there |
| `schedule_context_fetchers` gating | [[021-zeph-context/spec]] | Extends `ContextAssembler`'s fetcher scheduling with an opt-in active-type filter |
| `intent_scoped` reuse of `HeuristicRouter`, no new LLM call | [[024-multi-model-design/spec]] | Complies with the "no hardcoded model, resolve via provider registry" principle by adding no new LLM call at all for this feature |
| `FunctionalType` location in `zeph-common`, not `zeph-memory` | [[043-zeph-common/spec]] | Follows the existing no-`zeph-memory`-dependency boundary for `zeph-context` (issue #3665) |

---

## 8. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[004-memory/spec]] — Parent memory pipeline spec
- [[021-zeph-context/spec]] — `ContextAssembler`/`schedule_context_fetchers` this feature gates
- [[024-multi-model-design/spec]] — `*_provider`/no-hardcoded-model principle
- [[043-zeph-common/spec]] — Shared-primitives crate boundary rationale for `FunctionalType`'s placement
- GitHub issue #6086 (research) — closed by #6226
- Paper: MemGuard (arXiv:2605.28009)
