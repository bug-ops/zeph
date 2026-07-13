---
aliases:
  - Orchestration Node Control Parity Tasks
  - Node Timeout / Retry-Exhausted Recovery Tasks
  - Tasks 6021
tags:
  - sdd
  - tasks
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/075-orchestration-node-control-parity/plan]]"
  - "[[specs/075-orchestration-node-control-parity/spec]]"
---

# Task Breakdown: Orchestration Node Control Parity (GitHub #6021)

All tasks reference `[[specs/075-orchestration-node-control-parity/plan]]`. This is the developer's
primary implementation checklist alongside the architect/critic handoffs referenced there.
Implement in phase order; Phases 2-6 depend only on Phase 1, not on each other, and may be
parallelized across developers if desired. This document itself is a design artifact — per this
spec package's scope, no code is implemented as part of producing it (see spec.md §Out of Scope);
implementation is picked up by a future `new-feature`/`refactoring` team-develop session.

---

## Phase 1: Data Model

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T1.1 | Add `TimeoutPolicy { run_timeout_secs, idle_timeout_secs }` struct | P1-1 | `crates/zeph-orchestration/src/graph.rs` (or new module) | `idle_timeout_secs` doc comment states "not enforced in v1" |
| T1.2 | Add `RecoveryAction { state_injection }` struct | P1-1 | same | No `route_to` field — designed for additive extension later |
| T1.3 | Add `timeout: Option<TimeoutPolicy>` and `recovery: Option<RecoveryAction>` to `TaskNode` | P1-2 | `crates/zeph-orchestration/src/graph.rs:379-452` | `#[serde(default, skip_serializing_if = "Option::is_none")]` on both |
| T1.4 | Update `TaskNode` module doctest if it asserts the full field list | P1-2 | same | Only if the existing doctest pattern requires it |
| T1.5 | Unit tests: serde round-trip (`Some`/`Some`, `None`/`None`, pre-feature JSON with no keys) | P1-3 | `graph.rs` | Required coverage |

**Phase 1 gate:** `cargo nextest run -p zeph-orchestration` green before Phase 2-6.

---

## Phase 2: `validate()` Recovery Guards

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T2.1 | Add reject guard: `recovery.is_some() && verify_predicate.is_some()` | P2-1 | `crates/zeph-orchestration/src/dag.rs:37-91` (`validate()`'s per-task loop, `:51-77`) | `Err(OrchestrationError::InvalidGraph(...))` naming the task index |
| T2.2 | Add warn guard: `recovery.is_some()` under effective `Skip`/`Ask` strategy | P2-2 | same | `tracing::warn!`, does not reject; effective-strategy computation needs `graph.default_failure_strategy` threaded in — resolve the `validate(tasks: &[TaskNode], ...)` vs. `&TaskGraph` signature question as part of this task |
| T2.3 | Unit test: reject case | P2-3 | `dag.rs` | Blocking acceptance criterion (SRS FR-011) |
| T2.4 | Unit test: warn case for `Skip` | P2-3 | same | |
| T2.5 | Unit test: warn case for `Ask` | P2-3 | same | |
| T2.6 | Unit test: `recovery` under `Abort`/`Retry` → no warning, `Ok` | P2-3 | same | |

**Phase 2 gate:** `cargo nextest run -p zeph-orchestration` green before merge.

---

## Phase 3: Mode-1 Recovery in `propagate_failure()`

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T3.1 | Add `try_recover(graph, failed_id) -> bool` helper | P3-1 | `crates/zeph-orchestration/src/dag.rs` | Sets `Completed` + synthetic `TaskResult`; `tracing::info!` on success |
| T3.2 | Call `try_recover` at the top of the `Abort` arm | P3-1 | `dag.rs:243-253` | On `true`, return `Vec::new()` (no cancellations) |
| T3.3 | Call `try_recover` at the top of the retry-exhausted branch of the `Retry` arm | P3-1 | `dag.rs:281-298` | Same short-circuit behavior |
| T3.4 | Unit test: `Abort`-default + `state_injection` → `Completed`, correct `result.output`, `graph.status` unchanged | P3-2 | `dag.rs` | Blocking (SRS FR-007, BRD SC-04) |
| T3.5 | Unit test: retry-exhausted `Retry` + `state_injection` → same end-state | P3-2 | same | Blocking (SRS FR-007) |
| T3.6 | Unit test: `recovery == None` on both paths → existing Abort-equivalent behavior unchanged | P3-2 | same | Blocking regression (BRD SC-01) |
| T3.7 | Unit test: recovered task's dependent becomes eligible via `ready_tasks()`'s `Pending` arm | P3-2 | `dag.rs` | Blocking (SRS FR-008) |
| T3.8 | Test: `Skip`/`Ask` strategy with `recovery` configured still ends `Skipped`/`Paused`, not `Completed` | P3-2 | `dag.rs` | Confirms guards from Phase 2 hold at runtime too |

**Phase 3 gate:** `cargo nextest run -p zeph-orchestration` green before Phase 7.

---

## Phase 4: Per-Task Timeout — Spawned Tasks

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T4.1 | Add `effective_run_timeout(&self, task_id) -> Duration` | P4-1 | `crates/zeph-orchestration/src/scheduler/tick/mod.rs` | Falls back to `self.task_timeout` when no override |
| T4.2 | Use `effective_run_timeout` in `check_timeouts()`'s filter predicate | P4-1 | `tick/mod.rs:727-768` | Replaces uniform `self.task_timeout` comparison |
| T4.3 | Use `effective_run_timeout` in `wait_event()`'s nearest-deadline computation | P4-2 | `tick/mod.rs:254-270` | Requires iterating `self.running` as `(id, r)` pairs |
| T4.4 | Unit test: two running tasks, one overridden (short), one default (long) — only the overridden one times out early | P4-3 | `tick/mod.rs` | Blocking (SRS FR-002, BRD SC-02) |
| T4.5 | Unit test: `wait_event()`'s computed wait reflects the nearer per-task deadline | P4-3 | same | Blocking (SRS FR-003) |
| T4.6 | Regression test: no overrides anywhere → identical timing to pre-feature | P4-3 | same | Blocking (BRD SC-01) |

**Phase 4 gate:** `cargo nextest run -p zeph-orchestration` green before Phase 7.

---

## Phase 5: Per-Task Timeout — `RunInline` Tasks

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T5.1 | Compute `effective_run_timeout` for the `RunInline` task at dispatch | P5-1 | `crates/zeph-core/src/agent/scheduler_loop.rs:258-` | Falls back to the graph-global `task_timeout_secs` |
| T5.2 | Add third `tokio::select!` branch (timeout) alongside the existing tool-loop and cancellation-token arms | P5-1 | same | Produces `TaskOutcome::Failed` on fire |
| T5.3 | Integration test: short override + slow tool loop → timeout branch fires, `TaskOutcome::Failed` | P5-2 | `scheduler_loop.rs` tests | Blocking (SRS FR-004, BRD SC-02) |
| T5.4 | Regression test: no override + fast tool loop → completes normally, timeout branch never fires | P5-2 | same | Blocking (BRD SC-01) |
| T5.5 | Integration test: `RunInline` + `timeout` + `recovery` both configured — timeout fires, recovery applies, dependents unblock | P5-2 | same | Cross-phase (Phase 3 + Phase 5) |

**Phase 5 gate:** `cargo nextest run -p zeph-core --lib` green before Phase 7.

---

## Phase 6: Config — `default_idle_timeout_secs`

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T6.1 | Add `default_idle_timeout_secs: Option<u64>` field with `#[serde(default)]` | P6-1 | `crates/zeph-config/src/experiment.rs` | Doc comment states "RESERVED — not yet enforced" |
| T6.2 | Add named migration step (add-with-default) | P6-2 | `crates/zeph-config/src/migrate/mod.rs` (registry) + `steps.rs` (function) | Mirror a prior trivial add-only migration's shape |
| T6.3 | Add `--init` wizard prompt in `step_orchestration()` | P6-3 | `src/init/agents.rs:11` | Reserved/not-yet-enforced framing (NFR-OB-04) |
| T6.4 | Document field in `docs/src/` and any config.toml template | P6-4 | `docs/src/` | Same reserved wording |
| T6.5 | Unit test: default config → `None` | P6-5 | `experiment.rs` | |
| T6.6 | Unit test: TOML round-trip with explicit value | P6-5 | same | |
| T6.7 | Unit test: pre-feature config migrates cleanly to `None` | P6-5 | migration test module | Blocking (SRS FR-018, BRD SC-08) |

**Phase 6 gate:** `cargo nextest run -p zeph-config` green before Phase 7.

---

## Phase 7: Documentation and Mandatory Integration Points

| # | Task | Integration Point | Path | Notes |
|---|------|--------------------|------|-------|
| T7.1 | Doc-annotate `ready_tasks()`'s `Ready` arm as load-bearing for recovery unblock | — | `crates/zeph-orchestration/src/dag.rs:185-191` | SRS FR-020, exact wording in spec.md §4 |
| T7.2 | Doc-note on `OrchestrationMetrics`/`finalize_plan_*` confirming recovered-node status-derived counting | — | `crates/zeph-core/src/metrics.rs:104-105` or `crates/zeph-core/src/agent/plan.rs:722,801` | No code change — resolves SRS FR-021 |
| T7.3 | Confirm no CLI/TUI surface needed beyond config.toml/`--init` (#2/#3) | #2, #3 | PR description | Documented rationale, not silent — mirrors `task_timeout_secs` precedent |
| T7.4 | Create testing playbook | #6 | `/Users/rabax/Dev/zeph/.local/testing/playbooks/orchestration-node-control-parity.md` | Main-repo path; scenarios per plan.md P7-3 |
| T7.5 | Add coverage-status rows | #7 | `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` | Rows: per-task timeout (spawned + RunInline), Mode-1 recovery, validate() guards, config field — status `Untested` |
| T7.6 | Update `CHANGELOG.md` `[Unreleased]` | — | `CHANGELOG.md` | Root; note idle-timeout reserved status and Mode-2 deferral |
| T7.7 | Register spec in `specs/README.md` and `specs/MOC-specs.md` | — | `specs/` | **Outside this spec package's write scope (sdd role is restricted to `specs/075-orchestration-node-control-parity/`) — team-lead action, not a developer task** |

---

## Acceptance Criteria (for PR merge)

- [ ] All Phase 1-6 unit/integration tests pass: `cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler" --lib --bins`
- [ ] `cargo +nightly fmt --check`
- [ ] `cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`
- [ ] Rustdoc gate: `RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] Doc-tests: `cargo test --doc --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] Default-off regression tests present and passing (T1.5 pre-feature-JSON case, T3.6, T4.6, T5.4)
- [ ] Mode-1 recovery tests present and passing for both `Abort`-default and retry-exhausted `Retry` (T3.4, T3.5)
- [ ] `validate()` guard tests present and passing (T2.3-T2.6)
- [ ] Cascade-precedence behavior verified live or via integration test (no code change needed — existing ordering — but the PR description must state this was explicitly checked, not merely assumed)
- [ ] Async-supervision scan shows zero new `tokio::spawn()` sites introduced by this PR
- [ ] `CHANGELOG.md` updated
- [ ] Testing playbook + coverage-status rows added (main-repo `.local/testing/` path)
- [ ] `specs/README.md` and `specs/MOC-specs.md` register `orchestration-node-control-parity` (team-lead action)
- [ ] Follow-up issues filed for Mode 2 (`route_to` redesign, N5/N1/N3) and Alt A (idle-timeout progress-signal plumbing) — team-lead action, not part of this PR
