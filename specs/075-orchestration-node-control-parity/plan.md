---
aliases:
  - Orchestration Node Control Parity Plan
  - Node Timeout / Retry-Exhausted Recovery Plan
  - Plan 6021
tags:
  - sdd
  - plan
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/075-orchestration-node-control-parity/spec]]"
  - "[[specs/075-orchestration-node-control-parity/tasks]]"
---

# Implementation Plan: Orchestration Node Control Parity (GitHub #6021)

Source of truth: architect handoffs `.local/handoff/2026-07-13T20-47-55-architect.md` (base),
`.local/handoff/2026-07-13T21-00-15-architect.md`/`21-09-12-architect.md` (round-2 revisions),
`.local/handoff/2026-07-13T21-12-06-architect.md` (v3, final); critic-approved
`.local/handoff/2026-07-13T21-16-30-critic.md` (verdict **minor / approved**). This plan sequences
the change set from `[[specs/075-orchestration-node-control-parity/spec]]` §3 into an implementable
order. No architectural re-derivation — this is a formalization of the already-approved design.

**Decision Type:** `refactoring` (additive extension of an existing subsystem; no new crate).
**Structure:** `workspace` (existing); new code in `zeph-orchestration` (data model + `dag.rs` +
`tick/mod.rs` logic) and `zeph-config` (one new field + validation + migration); one new branch
in `zeph-core`'s `RunInline` `select!`. No cross-crate dependency added.

## Recommended Implementation Order

**Phase 1: `zeph-orchestration` — data model.** `TimeoutPolicy`/`RecoveryAction` types and the
`TaskNode.timeout`/`.recovery` fields. Implement first — every later phase reads these fields.

**Phase 2: `zeph-orchestration` — `validate()` guards.** Depends on Phase 1's field existing;
independently unit-testable with no scheduler dependency.

**Phase 3: `zeph-orchestration` — recovery in `propagate_failure()`.** Depends on Phase 1; the
core Mode-1 behavior, fully unit-testable against `TaskGraph`/`dag.rs` in isolation (no async, no
scheduler tick required).

**Phase 4: `zeph-orchestration` — per-task timeout in `check_timeouts()`/`wait_event()`.** Depends
on Phase 1; independent of Phase 3.

**Phase 5: `zeph-core` — `RunInline` timeout branch.** Depends on Phase 1 (reads
`TimeoutPolicy.run_timeout_secs`); the only cross-crate touch point besides config.

**Phase 6: `zeph-config` — `default_idle_timeout_secs` + integration.** Independent of Phases
2-5; can be implemented in parallel with them once Phase 1 is merged (or even before, since it
does not depend on `TaskNode`'s new fields).

**Phase 7: Documentation, doc-comment annotations, testing playbook, CHANGELOG.** Implement
last; lowest risk, most mechanical.

---

## Phase 1: Data Model

### P1-1: `TimeoutPolicy` and `RecoveryAction` types

**File:** `crates/zeph-orchestration/src/graph.rs` (co-located with `TaskNode`, or a new
`crates/zeph-orchestration/src/timeout_policy.rs` / `recovery.rs` module if `graph.rs` is judged
too large already — developer's call, consistent with existing module-splitting conventions in
the crate)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    pub run_timeout_secs: Option<u64>,
    pub idle_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryAction {
    pub state_injection: Option<String>,
}
```

Both `#[derive(Debug, Clone, Serialize, Deserialize)]`, both documented with `///` doc comments
per CLAUDE.md rustdoc rules, `idle_timeout_secs`'s doc comment explicitly states "not enforced in
v1 — reserved for a future progress-signal mechanism" (FR-005).

### P1-2: `TaskNode` fields

**File:** `crates/zeph-orchestration/src/graph.rs:379-452`

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub timeout: Option<TimeoutPolicy>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub recovery: Option<RecoveryAction>,
```

Insert after the existing `asset_sensitivity` field, following the same doc-comment style.
Update the module-level doctest at the top of `graph.rs` (the existing `TaskNode::new(...)`
example) if it asserts on the full field list — add `assert!(node.timeout.is_none())` /
`assert!(node.recovery.is_none())` alongside the existing `network_scope`/`asset_sensitivity`
assertions if that pattern is present.

### P1-3: Unit tests (Phase 1)

- Serde round-trip: a `TaskNode` with `timeout`/`recovery` both `Some(...)` round-trips through
  JSON; a `TaskNode` with both `None` serializes without the fields present at all
  (`skip_serializing_if`).
- Deserialize a pre-feature-shaped JSON blob (no `timeout`/`recovery` keys) — both fields default
  to `None` (`#[serde(default)]`).

**Phase 1 gate:** `cargo nextest run -p zeph-orchestration` green (graph module) before Phase 2/3.

---

## Phase 2: `validate()` Recovery Guards

### P2-1: Reject `recovery` + `verify_predicate`

**File:** `crates/zeph-orchestration/src/dag.rs:37-91` (`validate()`), inside the existing
per-task loop at `:51-77`

```rust
if task.recovery.is_some() && task.verify_predicate.is_some() {
    return Err(OrchestrationError::InvalidGraph(format!(
        "task {i} sets both recovery and verify_predicate — a predicate-gated task \
         must not be recovery-eligible"
    )));
}
```

### P2-2: Warn on `recovery` under `Skip`/`Ask`

Same loop. Compute the task's effective failure strategy
(`task.failure_strategy.unwrap_or(graph.default_failure_strategy)` — note `validate()` takes
`tasks: &[TaskNode]`, not `&TaskGraph`, so the effective-strategy computation needs the graph's
`default_failure_strategy` threaded in as a parameter, or the guard is added as a second pass
that takes `&TaskGraph` — developer's call on the cleanest signature, but the check must run at
the same validation boundary):

```rust
if task.recovery.is_some()
    && matches!(effective_strategy, FailureStrategy::Skip | FailureStrategy::Ask)
{
    tracing::warn!(
        task_index = i,
        strategy = ?effective_strategy,
        "recovery configured but effective failure strategy is Skip/Ask — recovery is inert"
    );
}
```

### P2-3: Unit tests (Phase 2)

- `recovery.is_some() && verify_predicate.is_some()` → `Err(InvalidGraph)`.
- `recovery.is_some()` alone → `Ok`.
- `recovery.is_some()` with effective strategy `Skip` → `Ok` (warns, does not reject) — assert on
  the warning via `tracing`'s test-capture mechanism if the crate has one, else assert only on
  `Ok` and treat the warning as a manual/live-test verification item.
- `recovery.is_some()` with effective strategy `Ask` → same as `Skip`.
- `recovery.is_some()` with effective strategy `Abort`/`Retry` → `Ok`, no warning.

**Phase 2 gate:** `cargo nextest run -p zeph-orchestration` green (dag module) before merge.

---

## Phase 3: Mode-1 Recovery in `propagate_failure()`

### P3-1: Recovery branch

**File:** `crates/zeph-orchestration/src/dag.rs:223-322` (`propagate_failure()`)

Attach the recovery check at the top of the `FailureStrategy::Abort` arm (`:243-253`) and at the
retry-exhausted fallthrough inside the `FailureStrategy::Retry` arm (`:281-298`) — both currently
converge on the same "mark graph Failed, collect Running tasks to cancel" shape, so the cleanest
implementation is a small shared helper:

```rust
fn try_recover(graph: &mut TaskGraph, failed_id: TaskId) -> bool {
    let Some(injection) = graph.tasks[failed_id.index()]
        .recovery
        .as_ref()
        .and_then(|r| r.state_injection.clone())
    else {
        return false;
    };
    let node = &mut graph.tasks[failed_id.index()];
    node.status = TaskStatus::Completed;
    node.result = Some(TaskResult {
        output: injection,
        artifacts: Vec::new(),
        duration_ms: 0,
        agent_id: None,
        agent_def: Some("__recovery__".to_string()),
    });
    tracing::info!(task_id = %failed_id, "orchestration.dag.recover_task: Mode-1 recovery applied");
    true
}
```

Call `try_recover(graph, failed_id)` at the top of both the `Abort` arm and the retry-exhausted
branch of the `Retry` arm; on `true`, `return Vec::new()` (no tasks to cancel — the node
recovered, `graph.status` is untouched, so no `Running` task needs cancellation as a side effect
of this specific node's failure). On `false`, fall through to the existing behavior unchanged.

### P3-2: Unit tests (Phase 3)

- `Abort`-default failure, `recovery.state_injection = Some(v)` configured → node ends
  `Completed`, `result.output == v`, `graph.status` unchanged (still `Running` if it was).
- Retry-exhausted `Retry` failure, `recovery.state_injection = Some(v)` configured → same
  end-state as above.
- Either case with `recovery == None` → existing Abort-equivalent behavior, byte-identical to
  pre-feature (regression test, BRD SC-01).
- Recovery + a dependent task: dependent's `depends_on` includes the recovered task; after
  recovery, `ready_tasks()` includes the dependent (`Pending`→ eligible via the `Pending` arm's
  `depends_on` completion check).
- `Skip`/`Ask` arms are never affected — assert `try_recover` is not called from those arms
  (structural/code-review-level check, or an integration test asserting a `Skip`-strategy node
  with `recovery` configured still ends `Skipped`, not `Completed`).

**Phase 3 gate:** `cargo nextest run -p zeph-orchestration` green (dag module) before Phase 7.

---

## Phase 4: Per-Task Timeout — Spawned Tasks

### P4-1: `check_timeouts()` effective timeout

**File:** `crates/zeph-orchestration/src/scheduler/tick/mod.rs:727-768`

```rust
fn effective_run_timeout(&self, task_id: TaskId) -> Duration {
    self.graph.tasks[task_id.index()]
        .timeout
        .as_ref()
        .and_then(|t| t.run_timeout_secs)
        .map(Duration::from_secs)
        .unwrap_or(self.task_timeout)
}
```

Replace the existing `r.started_at.elapsed() > self.task_timeout` filter predicate (inside the
`self.running.iter().filter(...)` closure) with `r.started_at.elapsed() >
self.effective_run_timeout(*id)`.

### P4-2: `wait_event()` per-task nearest-deadline

**File:** `crates/zeph-orchestration/src/scheduler/tick/mod.rs:254-270`

Replace the `self.task_timeout.checked_sub(r.started_at.elapsed())` inside the `.map(...)` closure
(`:264-268`) with `self.effective_run_timeout(id).checked_sub(r.started_at.elapsed())` — note this
requires iterating `self.running` as `(id, r)` pairs rather than `.values()` alone, since
`effective_run_timeout` needs the `TaskId` to look up the per-task override.

### P4-3: Unit tests (Phase 4)

- Two running tasks, one with a short `run_timeout_secs` override, one with none — only the
  overridden task times out at the short interval; the other respects the (longer) global
  default.
- `wait_event()`'s computed `wait_duration` reflects the nearer of the two effective deadlines,
  not the uniform global one.
- Regression: no per-task overrides configured anywhere → identical timing behavior to
  pre-feature code (BRD SC-01).

**Phase 4 gate:** `cargo nextest run -p zeph-orchestration` green (tick module) before Phase 7.

---

## Phase 5: Per-Task Timeout — `RunInline` Tasks

### P5-1: Third `tokio::select!` branch

**File:** `crates/zeph-core/src/agent/scheduler_loop.rs:258-`

```rust
let effective_run_timeout = task.timeout.as_ref()
    .and_then(|t| t.run_timeout_secs)
    .map(Duration::from_secs)
    .unwrap_or(self.services.orchestration.orchestration_config.task_timeout_secs_as_duration());
    // exact accessor name/shape for the graph-global fallback is an implementation
    // detail — mirror however task_timeout is currently threaded into this scope

let outcome = tokio::select! {
    result = self.run_inline_tool_loop(&prompt, max_iter) => { /* existing arm, unchanged */ }
    () = cancel_token.cancelled() => { /* existing arm, unchanged */ }
    () = tokio::time::sleep(effective_run_timeout) => {
        zeph_orchestration::TaskOutcome::Failed {
            error: format!("RunInline task exceeded run_timeout ({effective_run_timeout:?})"),
        }
    }
};
```

(`tokio::time::sleep` inside `select!` is equivalent to `tokio::time::timeout` wrapping the whole
arm set here, and avoids restructuring the other two arms — developer's call on which idiom reads
cleaner in context; both satisfy FR-004.)

### P5-2: Unit/integration tests (Phase 5)

- A `RunInline` task with a short `run_timeout_secs` override and a tool loop that would run
  longer → the timeout branch fires, produces `TaskOutcome::Failed`, and downstream handling
  (`propagate_failure`, recovery if configured) proceeds identically to a spawned-task timeout.
- A `RunInline` task with no override and a fast-completing tool loop → completes normally,
  timeout branch never fires (regression, BRD SC-01).
- Integration test combining Phase 3 + Phase 5: a `RunInline` task with both `timeout` and
  `recovery` configured — timeout fires, recovery applies, dependents unblock.

**Phase 5 gate:** `cargo nextest run -p zeph-core --lib` (scheduler_loop tests) green before Phase 7.

---

## Phase 6: Config — `default_idle_timeout_secs`

### P6-1: Config field

**File:** `crates/zeph-config/src/experiment.rs`, sibling to `task_timeout_secs` (`:274`)

```rust
/// Global default idle/no-progress timeout in seconds. RESERVED — not yet enforced;
/// see the orchestration-node-control-parity spec's Alt A follow-up. `None` = off.
#[serde(default)]
pub default_idle_timeout_secs: Option<u64>,
```

### P6-2: Migration step

**File:** `crates/zeph-config/src/migrate/mod.rs` (new step function, likely in
`crates/zeph-config/src/migrate/steps.rs` alongside other named steps per the existing pattern),
registered in the `MIGRATIONS` vec (`:646-`)

Add-with-default step: existing configs gain `default_idle_timeout_secs = None` (i.e., the key is
simply absent — `#[serde(default)]` already handles this on load; the migration step exists
mainly to be an explicit, documented, named entry in the registry consistent with this project's
"every new config field gets a migration step" convention) — mirror the shape of a prior
similarly-trivial add-only migration (e.g. `MigrateOrchestrationAssetSensitivity`) rather than
inventing a new migration idiom.

### P6-3: `--init` wizard

**File:** `src/init/agents.rs:11` (`step_orchestration()`)

Add a prompt, framed as reserved/not-yet-enforced (FR-005/NFR-OB-04):

> "Idle-timeout (no-progress) detection — reserved for a future release, not yet enforced. Leave
> unset unless you want the value persisted for when this ships. [blank/skip default]"

### P6-4: `config.toml` documentation

Document the field in `docs/src/` (per branching.md's PR checklist) and inline in any
`config.toml` example/template file the project ships, with the same "reserved — not yet
enforced" wording.

### P6-5: Unit tests (Phase 6)

- Default config: `default_idle_timeout_secs == None`.
- TOML round-trip: explicit value set → round-trips correctly.
- Migration test: a pre-feature config (no key present) migrates to `default_idle_timeout_secs ==
  None` with the new step recorded as a no-op/trivial change in the migration diff.

**Phase 6 gate:** `cargo nextest run -p zeph-config` green before Phase 7.

---

## Phase 7: Documentation and Mandatory Integration Points

### P7-1: `ready_tasks()` doc annotation

**File:** `crates/zeph-orchestration/src/dag.rs:185-191` (the `Ready` arm)

Add a doc comment (or extend the existing one) stating this arm's predicate-only bypass (no
`depends_on` re-check) is load-bearing for the recovery unblock path, per SRS FR-020's exact
wording.

### P7-2: Recovered-node metrics note

No code change required (FR-021 is resolved by existing status-derived counting) — add a short
doc comment on `OrchestrationMetrics.tasks_completed`/`tasks_failed`
(`crates/zeph-core/src/metrics.rs:104-105`) or on `finalize_plan_completed`/`finalize_plan_failed`
(`crates/zeph-core/src/agent/plan.rs:722,801`) noting that a Mode-1-recovered node is correctly
counted as completed by construction (final-status-derived counting), so future contributors do
not "fix" this into an event-time increment that would double-count or miscount it.

### P7-3: Mandatory integration points checklist

| # | Point | Where |
|---|-------|-------|
| 1 | `config.toml` section | `[orchestration]` gains `default_idle_timeout_secs` — documented in `docs/src/` (P6-4) |
| 2 | CLI subcommand/argument | N/A — passive config default, no dedicated CLI surface, consistent with `task_timeout_secs` precedent |
| 3 | TUI command palette / `/` command | N/A for the config field (same rationale as #2); no background/implicit operation is introduced by this feature that would need a TUI status spinner — recovery is a synchronous data mutation, not a background operation |
| 4 | `--init` wizard | New reserved-field prompt in `step_orchestration()` (P6-3) |
| 5 | `--migrate-config` | New named step (P6-2) |
| 6 | Testing playbook | Create `/Users/rabax/Dev/zeph/.local/testing/playbooks/orchestration-node-control-parity.md` (main-repo path) — scenarios: default-off regression, per-task run_timeout (spawned + RunInline), idle-timeout no-op verification, Mode-1 recovery (Abort + retry-exhausted), cascade-precedence, validate() reject/warn, metrics classification |
| 7 | Coverage status | Add rows in `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` for: per-task timeout override, RunInline timeout branch, Mode-1 recovery, validate() guards, `default_idle_timeout_secs` config — status `Untested` |

### P7-4: CHANGELOG.md

Add an `[Unreleased]` entry describing the per-task timeout override and Mode-1 recovery
capability, noting the idle-timeout field is reserved/not-yet-enforced and that Mode 2 is
deferred to a follow-up issue.

---

## Pre-Merge Checklist

- [ ] `cargo +nightly fmt --check`
- [ ] `cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`
- [ ] `cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler" --lib --bins`
- [ ] `RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] `cargo test --doc --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] Async-supervision scan (`.claude/rules/continuous-improvement.md`) confirms zero new `tokio::spawn()` sites
- [ ] `CHANGELOG.md` updated (`[Unreleased]`)
- [ ] `.local/testing/playbooks/orchestration-node-control-parity.md` created (main-repo path)
- [ ] `.local/testing/coverage-status.md` rows added (main-repo path)
- [ ] LLM serialization gate: N/A — no LLM request/response serialization path is touched by this feature (recovery injects a planner-authored literal, not an LLM-generated value); confirm and record in the PR description
- [ ] `specs/README.md` and `specs/MOC-specs.md` register `orchestration-node-control-parity` (team-lead: outside this spec package's write scope — see handoff)
