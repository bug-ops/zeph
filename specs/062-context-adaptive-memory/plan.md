---
aliases:
  - CAM Implementation Plan
tags:
  - plan
  - context
  - memory
created: 2026-05-28
status: approved
related:
  - "[[062-context-adaptive-memory/spec]]"
  - "[[062-context-adaptive-memory/tasks]]"
---

# Context-Adaptive Memory — Implementation Plan

## Phase 1: MVP (v0.21)

All Phase 1 work targets issues #4016, #4017, #4018.

### Step 1 — Foundation types in zeph-common

**Crate**: `zeph-common`
**Module**: `src/fidelity.rs` (new file)
**Exports**: `ContextFidelity`, `PlannedToolHint`

Deliverables:
- `ContextFidelity` enum (`#[repr(u8)]`, `Default = Full`, `Copy + Clone + PartialEq + Eq + Hash`)
- `PlannedToolHint` struct with `tool_name`, `keywords`, `distance_from_current`
- Public re-export from `zeph_common::prelude` (if the crate uses one)
- Full doc comments with `# Examples` and doc-tests

No dependencies on other `zeph-*` crates.

### Step 2 — MessageMetadata extension in zeph-llm

**Crate**: `zeph-llm`
**Module**: wherever `MessageMetadata` is defined

Deliverables:
- Add `pub fidelity_tag: Option<ContextFidelity>` with `#[serde(default)]`
- Import `ContextFidelity` from `zeph-common`
- Verify `zeph-llm` Cargo.toml already has `zeph-common.workspace = true`

No behavior change — this field starts as `None` everywhere.

### Step 3 — FidelityConfig and FidelityScorer in zeph-context

**Crate**: `zeph-context`
**Module**: `src/fidelity.rs` (new file)

Deliverables:
- `FidelityConfig` struct with all fields from spec.md §8.1 + `#[serde(default)]`
- `FidelityScorer` struct (stateless, takes config reference)
- `FidelityScorer::score_and_apply()` — the main entry point
- Internal helpers: `score_message()`, `resolve_fidelity()`, `apply_rendering()`, `merge_same_role_placeholders()`, `identify_tool_pairs()`
- Tracing spans: `context.fidelity.score`, `context.fidelity.apply`, `context.fidelity.merge`
- Unit test module with ≥ 10 test cases (see AC-01)

Scoring algorithm must implement:
- Weight normalization (INV-05, FR-008 through FR-010)
- Short query fallback (FR-009)
- Tool pair atomicity (FR-020 through FR-022)
- Consecutive same-role merge (FR-023 through FR-025)
- All exempt message rules (FR-015 through FR-018)

Performance constraint: must pass AC-11 benchmark.

### Step 4 — ContextAssemblyInput extension in zeph-context

**Crate**: `zeph-context`
**Module**: `src/input.rs`

Deliverables:
- Add `planned_next_tools: &'a [PlannedToolHint]` with `#[serde(skip)]` / lifetime-bound
- Add `fidelity_config: Option<&'a FidelityConfig>`
- Update all `ContextAssemblyInput` construction sites to pass defaults

### Step 5 — ContextManager extensions in zeph-context

**Crate**: `zeph-context`
**Module**: `src/manager.rs`

Deliverables:
- Add `pub(crate) regraded_this_turn: bool` field (initialized `false`)
- Implement `should_proactively_regrade(&self, cached_tokens: u64) -> bool` (guard chain from spec.md §5.1)
- Update `advance_turn()` to reset `regraded_this_turn = false`
- Tracing span: `context.fidelity.regrade`

### Step 6 — Wire fidelity scoring in zeph-agent-context

**Crate**: `zeph-agent-context`
**Module**: `src/service.rs`

Deliverables:
- Change `apply_prepared_context()` return type to `(ContextDelta, usize)` where `usize` is `inserted_count`
- Implement incremental insertion counting across all message insertion paths
- After `apply_prepared_context()` returns, call `FidelityScorer::score_and_apply()` when guards are satisfied
- TUI spinner: emit `send_status("Scoring context fidelity…")` before scorer call

### Step 7 — Proactive regrade call site in zeph-agent-context

**Crate**: `zeph-agent-context`
**Module**: `src/summarization/scheduling.rs`

Deliverables:
- In `maybe_compact()` (or equivalent), before tier dispatch, call `should_proactively_regrade()`
- When trigger fires, re-run `FidelityScorer::score_and_apply()` and set `regraded_this_turn = true`

### Step 8 — Placeholder exclusion in hard compaction

**Crate**: `zeph-agent-context`
**Module**: `src/summarization/compaction.rs`

Deliverables:
- In the message selection loop, add: `if msg.metadata.fidelity_tag == Some(ContextFidelity::Placeholder) { continue; }`
- Unit test: verify no Placeholder messages appear in summarizer input

### Step 9 — Config integration

**Crate**: `zeph-config`

Deliverables:
- Add `FidelityConfigToml` (or equivalent) to the `[context]` section
- Default: `enabled = false`, all weights/thresholds at spec defaults
- Wire through `--migrate-config` if any existing `[context]` fields are renamed
- Add `[context.fidelity]` section to config documentation and `--init` wizard branch
- Update `config.toml` examples / `.local/config/testing.toml` with the new section

### Step 10 — Testing and observability

Deliverables:
- Unit tests for all 12 AC from spec.md §11
- Integration test: full context assembly round-trip with `enabled = true`
- Benchmark in `zeph-bench` for AC-11 (< 2ms for 500 messages)
- Update `.local/testing/playbooks/context-adaptive-memory.md` with test scenarios
- Update `.local/testing/coverage-status.md` with new rows (status: Untested)

---

## Phase 2: Deferred Implementation

These items are spec'd but NOT implemented in v0.21.

### P2-A — Orchestration DAG live wiring (PAACE full)

**Crate**: `zeph-orchestration`
**Prerequisite**: `zeph-orchestration` exposes a `lookahead_tools(depth: u8) -> Vec<PlannedToolHint>` method.

Implementation: populate `planned_next_tools` in `ContextAssemblyInput` from the active DAG at turn start.

### P2-B — Fidelity persistence to SQLite

Store the resolved `ContextFidelity` per-message in the session database. Allows cross-turn fidelity stability (a message that was Compressed stays Compressed unless explicitly regraded).

### P2-C — LLM-assisted Compressed rendering

When `deferred_summary` is absent, use a fast LLM provider to generate a high-quality summary of the message content instead of simple truncation. Requires `fidelity_compress_provider` config field.

### P2-D — Embedding-based semantic scoring

Replace `keyword_overlap()` with cosine similarity against a cached embedding of the current query. Requires the scorer to accept an `EmbeddingProvider` and become async.

---

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `inserted_count` hardcoded incorrectly | Medium | High (wrong exempt set) | Unit test asserting all insertion paths are counted |
| `compressed_max_tokens = 50` too aggressive for tool results | High | Medium (loss of useful context) | Post-merge live testing; easy config adjustment |
| Tool pair detection by `tool_call_id` mismatches | Low | High (API 400 errors) | Unit test with malformed pair IDs |
| Scoring overhead exceeds 2ms for large windows | Low | Medium | `max_scored_messages` cap + benchmark in AC-11 |
