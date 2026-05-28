---
aliases:
  - CAM Tasks
tags:
  - tasks
  - context
  - memory
created: 2026-05-28
status: approved
related:
  - "[[062-context-adaptive-memory/plan]]"
  - "[[062-context-adaptive-memory/spec]]"
---

# Context-Adaptive Memory — Implementation Tasks

Tasks follow plan.md step order. Each task is self-contained and assignable to a single developer session.

---

## T-01 — Add ContextFidelity and PlannedToolHint to zeph-common

**Crate**: `zeph-common`
**Files**: `crates/zeph-common/src/fidelity.rs` (new), `crates/zeph-common/src/lib.rs`

**Specification reference**: spec.md §4.1, §4.2; srs.md FR-001, FR-033; NFR-D01

**Acceptance**:
- `ContextFidelity` enum with `Full=0`, `Compressed=1`, `Placeholder=2`, `#[repr(u8)]`, `Default=Full`
- `PlannedToolHint` struct with `tool_name: String`, `keywords: Vec<String>`, `distance_from_current: u8`
- Both types: `Debug`, `Clone`, `PartialEq` (Eq for ContextFidelity), `serde::Serialize + Deserialize`
- Public re-export from `crates/zeph-common/src/lib.rs`
- Doc comments with `/// # Examples` and passing doc-tests
- `cargo nextest run -p zeph-common --doc` passes
- `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-common` passes

**Blocked by**: nothing

---

## T-02 — Add fidelity_tag to MessageMetadata in zeph-llm

**Crate**: `zeph-llm`
**Files**: wherever `MessageMetadata` is defined (find with `grep -r "MessageMetadata" crates/zeph-llm/`)

**Specification reference**: spec.md §9.6; srs.md NFR-D02, NFR-B02

**Acceptance**:
- `pub fidelity_tag: Option<ContextFidelity>` field added
- `#[serde(default)]` attribute present so existing sessions deserialize without error
- Verify `zeph-llm/Cargo.toml` has `zeph-common.workspace = true`; add if missing
- No behavior change — field starts as `None` in all construction sites
- `cargo nextest run -p zeph-llm` passes

**Blocked by**: T-01

---

## T-03 — Implement FidelityConfig and FidelityScorer in zeph-context

**Crate**: `zeph-context`
**Files**: `crates/zeph-context/src/fidelity.rs` (new), `crates/zeph-context/src/lib.rs`

**Specification reference**: spec.md §4.3, §4.4, §5, §6, §7; srs.md FR-002 through FR-025, FR-034; AC-01 through AC-05

**Acceptance**:
- `FidelityConfig` struct with all fields from spec.md §8.1, `serde::Deserialize`, `#[serde(default)]` on all fields
- `FidelityScorer` (stateless struct) with method:
  ```rust
  pub fn score_and_apply(
      &self,
      messages: &mut Vec<Message>,
      query: &str,
      planned_tools: &[PlannedToolHint],
      config: &FidelityConfig,
      tc: &dyn TokenCounting,
      inserted_count: usize,
  )
  ```
- Implements: weight normalization (INV-05), short query fallback (FR-009), tool pair atomicity (INV-03), consecutive same-role merge (INV-04), exempt message set (FR-015 through FR-018)
- Tracing spans: `context.fidelity.score`, `context.fidelity.apply`, `context.fidelity.merge`
- All scores in `[0.0, 1.0]` (property: tested with extreme inputs)
- Unit test module with ≥ 10 tests covering AC-01 cases:
  - Empty window → no change
  - All-exempt window → no downgrade
  - Tool pair atomicity (divergent scores → min applied)
  - Same-role merge (5 consecutive assistant Placeholder → merged to 1)
  - Score normalization (all signal subsets)
  - Short query fallback (`query.len() < 8`)
  - MemoryFirst bypass (guard tested)
  - `enabled = false` guard
  - Token count uses `tc.count_tokens()` for Placeholder rendering
  - Compressed rendering: truncation primary, `deferred_summary` optimization
- `cargo nextest run -p zeph-context -E 'test(fidelity)'` passes

**Blocked by**: T-01, T-02

---

## T-04 — Extend ContextAssemblyInput in zeph-context

**Crate**: `zeph-context`
**Files**: `crates/zeph-context/src/input.rs`

**Specification reference**: spec.md §9.1; srs.md FR-035

**Acceptance**:
- Two new fields added to `ContextAssemblyInput`:
  ```rust
  pub planned_next_tools: &'a [PlannedToolHint],
  pub fidelity_config: Option<&'a FidelityConfig>,
  ```
- All existing construction sites updated to pass `planned_next_tools: &[]` and `fidelity_config: None`
- No functional change when both fields are at their defaults
- `cargo nextest run --workspace --lib --bins` passes

**Blocked by**: T-01, T-03

---

## T-05 — Add proactive regrade support to ContextManager

**Crate**: `zeph-context`
**Files**: `crates/zeph-context/src/manager.rs`

**Specification reference**: spec.md §5.1, §9.2; srs.md FR-026 through FR-030; AC-07, AC-08

**Acceptance**:
- Field `pub(crate) regraded_this_turn: bool` added, initialized to `false`
- Method implemented:
  ```rust
  pub fn should_proactively_regrade(&self, cached_tokens: u64) -> bool
  ```
  Implements full guard chain: `regraded_this_turn`, `is_exhausted()`, `server_compaction_active` at 95%
- `advance_turn()` resets `regraded_this_turn = false`
- Unit tests for AC-07 and AC-08
- `cargo nextest run -p zeph-context` passes

**Blocked by**: T-01

---

## T-06 — Wire fidelity scoring in zeph-agent-context service

**Crate**: `zeph-agent-context`
**Files**: `crates/zeph-agent-context/src/service.rs`

**Specification reference**: spec.md §5, §9.3; srs.md FR-006, FR-014, FR-019; INV-01; AC-02

**Acceptance**:
- `apply_prepared_context()` return type changed to `(ContextDelta, usize)`
- `inserted_count` computed incrementally across ALL insertion paths (graph_facts, doc_rag, corrections, recall, cross_session, summaries, persona, trajectory, tree, reasoning, code_context, session_digest) — not hardcoded
- After `apply_prepared_context()` returns, call `FidelityScorer::score_and_apply()` when:
  - `fidelity_config.enabled == true`
  - `memory_first == false`
- TUI status emitted before scorer: `send_status("Scoring context fidelity…")`
- Unit test: mock `apply_prepared_context` with known insertion count, verify exempt set built correctly (AC-12)
- `cargo nextest run -p zeph-agent-context` passes

**Blocked by**: T-03, T-04, T-05

---

## T-07 — Wire proactive regrade trigger in summarization scheduling

**Crate**: `zeph-agent-context`
**Files**: `crates/zeph-agent-context/src/summarization/scheduling.rs`

**Specification reference**: spec.md §5.1; srs.md FR-026 through FR-030; INV-06; AC-08

**Acceptance**:
- In `maybe_compact()` (or equivalent), BEFORE tier dispatch, call `should_proactively_regrade()`
- When trigger fires: re-run `FidelityScorer::score_and_apply()`, set `regraded_this_turn = true`, call `recompute_prompt_tokens()`
- Does NOT set `CompactedThisTurn`
- Tracing span: `context.fidelity.regrade` emitted with `{budget_ratio, full_count, compressed_count, placeholder_count}`
- Unit test: AC-08 (double-regrade prevention)
- `cargo nextest run -p zeph-agent-context` passes

**Blocked by**: T-05, T-06

---

## T-08 — Exclude Placeholder messages from hard compaction

**Crate**: `zeph-agent-context`
**Files**: `crates/zeph-agent-context/src/summarization/compaction.rs`

**Specification reference**: spec.md §9.5; srs.md FR-031, FR-032; INV-02; AC-06

**Acceptance**:
- Message selection loop skips messages where `metadata.fidelity_tag == Some(ContextFidelity::Placeholder)`
- Compressed messages are NOT skipped
- Unit test: AC-06 (seed window with Placeholder messages, assert none in summarizer input)
- `cargo nextest run -p zeph-agent-context` passes

**Blocked by**: T-02, T-06

---

## T-09 — Config integration

**Crates**: `zeph-config`, `zeph-core`
**Files**: config type definitions, `--init` wizard, `--migrate-config`

**Specification reference**: spec.md §8; plan.md Step 9

**Acceptance**:
- `[context.fidelity]` config section accepted by `zeph-config`
- All fields default to spec.md §8.1 values
- `enabled = false` by default
- `--init` wizard includes `context.fidelity.enabled` prompt
- `.local/config/testing.toml` updated with commented-out `[context.fidelity]` example
- `cargo nextest run -p zeph-config` passes

**Blocked by**: T-03

---

## T-10 — Testing, benchmark, and playbook

**Crates**: `zeph-bench`, documentation
**Files**: `.local/testing/playbooks/context-adaptive-memory.md`, `.local/testing/coverage-status.md`

**Specification reference**: spec.md §11 AC-01 through AC-12; plan.md Step 10; NFR-P01 (AC-11)

**Acceptance**:
- Benchmark in `zeph-bench` for AC-11: scoring 500 messages < 2ms
- Live test playbook created at `.local/testing/playbooks/context-adaptive-memory.md`:
  - Test scenario 1: enable CAM, run 30-turn session, verify no mid-task blowout
  - Test scenario 2: verify token reduction vs. baseline without CAM
  - Test scenario 3: verify `enabled = false` produces identical behavior
  - Edge cases: tool-heavy sessions, very short queries, MemoryFirst mode
- Integration test verifies correct behavior when `enabled = false` (feature disabled path, AC-10)
- Coverage status rows added to `.local/testing/coverage-status.md` with status `Untested` for:
  - `FidelityScorer` (zeph-context)
  - `AgeMem proactive regrade` (zeph-agent-context)
  - `Placeholder exclusion in compaction` (zeph-agent-context)
  - `PAACE plan hints` (zeph-context)

**Blocked by**: T-06, T-07, T-08

---

## Summary Table

| Task | Description | Blocked by | Crate |
|---|---|---|---|
| T-01 | ContextFidelity + PlannedToolHint types | — | zeph-common |
| T-02 | MessageMetadata.fidelity_tag | T-01 | zeph-llm |
| T-03 | FidelityConfig + FidelityScorer | T-01, T-02 | zeph-context |
| T-04 | ContextAssemblyInput extension | T-01, T-03 | zeph-context |
| T-05 | ContextManager regrade support | T-01 | zeph-context |
| T-06 | Wire scoring in service.rs | T-03, T-04, T-05 | zeph-agent-context |
| T-07 | Proactive regrade trigger | T-05, T-06 | zeph-agent-context |
| T-08 | Placeholder exclusion in compaction | T-02, T-06 | zeph-agent-context |
| T-09 | Config integration | T-03 | zeph-config |
| T-10 | Benchmark + playbook | T-06, T-07, T-08 | zeph-bench + docs |
