---
aliases:
  - Orchestration Node Control Parity SRS
  - Node Timeout / Retry-Exhausted Recovery SRS
  - SRS 6021
tags:
  - sdd
  - srs
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/075-orchestration-node-control-parity/brd]]"
  - "[[specs/075-orchestration-node-control-parity/spec]]"
  - "[[specs/075-orchestration-node-control-parity/nfr]]"
---

# SRS: Orchestration Node Control Parity — Per-Task Timeouts and Retry-Exhausted Recovery (GitHub #6021)

ISO/IEC/IEEE 29148:2018 compliant. Requirements use EARS notation. Technical basis: architect
handoffs `.local/handoff/2026-07-13T20-47-55-architect.md` (base plan),
`.local/handoff/2026-07-13T21-00-15-architect.md` and `.local/handoff/2026-07-13T21-09-12-architect.md`
(round-2 revisions), `.local/handoff/2026-07-13T21-12-06-architect.md` (v3, final — Mode 2 dropped,
resume re-scan dropped); critic handoffs `.local/handoff/2026-07-13T20-51-35-critic.md` (round 1),
`.local/handoff/2026-07-13T21-12-00-critic.md` (N5 correction), `.local/handoff/2026-07-13T21-16-30-critic.md`
(final verdict: minor / approved, two non-blocking notes M-a/M-b folded in below). All code
citations verified against HEAD `d93d82e8`.

## 1. Scope

This SRS specifies v1 of the orchestration node control parity capability: a per-task run-timeout
override, a config-surfaced-but-inert `idle_timeout_secs` field, and a single declarative
"substitute output and continue" (Mode 1) recovery mechanism for terminal `Abort`-default or
retry-exhausted `Retry` failures. Mode 2 (reroute-to-alternate-node) and the idle-timeout
progress-signal plumbing are explicitly deferred (§8).

---

## 2. Per-Task Timeout Override

### FR-001: `TimeoutPolicy` Data Model on `TaskNode`

**THE SYSTEM SHALL** add a nested `TimeoutPolicy { run_timeout_secs: Option<u64>,
idle_timeout_secs: Option<u64> }` type and expose it as `timeout: Option<TimeoutPolicy>` on
`TaskNode` (`crates/zeph-orchestration/src/graph.rs:379-452`), annotated
`#[serde(default, skip_serializing_if = "Option::is_none")]` consistent with the existing
`network_scope`/`asset_sensitivity` forward-compatibility pattern on the same struct.

### FR-002: Per-Task Run-Timeout Enforcement — Spawned Tasks

**WHEN** `check_timeouts()` (`crates/zeph-orchestration/src/scheduler/tick/mod.rs:727-768`)
evaluates a running spawned task, **THE SYSTEM SHALL** compute an effective run-timeout as
`task.timeout.and_then(|t| t.run_timeout_secs).map(Duration::from_secs)`, falling back to the
existing graph-global `self.task_timeout` **WHEN** no per-task override is set.

**THE SYSTEM SHALL** compute this effective timeout in `O(1)` per running task — **THE SYSTEM
SHALL NOT** introduce a full-graph scan inside `check_timeouts()`'s existing `O(self.running)`
loop.

### FR-003: `wait_event()` Nearest-Deadline Becomes Per-Task-Aware

**THE SYSTEM SHALL** update the nearest-timeout computation in `wait_event()`
(`crates/zeph-orchestration/src/scheduler/tick/mod.rs:261-270`, currently
`self.task_timeout.checked_sub(r.started_at.elapsed())` applied uniformly) to use each running
task's effective run-timeout (FR-002) instead of the single global `self.task_timeout`. **THE
SYSTEM SHALL** preserve the existing `O(self.running)` complexity of this computation — no
additional graph traversal per call.

### FR-004: Per-Task Run-Timeout Enforcement — `RunInline` Tasks

**THE SYSTEM SHALL** add a third branch to the inline `tokio::select!` in the `RunInline`
execution path (`crates/zeph-core/src/agent/scheduler_loop.rs:258-`) that races
`tokio::time::timeout(effective_run_timeout, self.run_inline_tool_loop(...))` alongside the
existing tool-loop and cancellation-token branches.

> **Rationale:** `check_timeouts()` runs on the scheduler's tick loop, which is blocked for the
> entire duration of a `RunInline` task's execution — `check_timeouts()` structurally never fires
> while a `RunInline` task is in flight. The inline `select!` is the only structurally viable
> enforcement site for this task kind.

**WHEN** the `tokio::time::timeout` branch fires before the tool loop completes, **THE SYSTEM
SHALL** treat the outcome identically to the existing cancellation-token branch's `Failed`
outcome path (same `zeph_orchestration::TaskOutcome::Failed` construction), so downstream failure
handling (`propagate_failure`, recovery per §4) is uniform across both dispatch kinds.

### FR-005: `idle_timeout_secs` — Defined, Config-Surfaced, Documented No-Op in v1

**THE SYSTEM SHALL** expose `idle_timeout_secs` as a serializable, config-surfaced field (on both
`TimeoutPolicy` per-task and a new graph-global default, FR-015) with its target semantics fully
documented (an idle/no-progress cap, distinct from the hard `run_timeout_secs` cap).

**THE SYSTEM SHALL NOT** enforce `idle_timeout_secs` in v1 — no progress-signal mechanism exists
in `zeph-subagent` or `zeph-orchestration` today (verified: zero heartbeat/liveness/progress hits
across both crates), so there is no signal to evaluate the field against.

**THE SYSTEM SHALL** mark this field's inert status loudly wherever an operator could configure
it: the `--init` wizard help text and the `config.toml` comment for both the per-task and the
graph-global field **SHALL** state "reserved — not yet enforced (see follow-up)" (critic finding
M-b), so a user who sets it does not assume idle-based kills are active.

---

## 3. Terminal-Failure Recovery (Mode 1: `state_injection`)

### FR-006: `RecoveryAction` Data Model on `TaskNode`

**THE SYSTEM SHALL** add `RecoveryAction { state_injection: Option<String> }` and expose it as
`recovery: Option<RecoveryAction>` on `TaskNode`
(`crates/zeph-orchestration/src/graph.rs:379-452`), annotated
`#[serde(default, skip_serializing_if = "Option::is_none")]`.

**THE SYSTEM SHALL NOT** add a `route_to` field to `RecoveryAction` in v1 (Mode 2 is deferred,
§8) — **THE SYSTEM SHALL** design `RecoveryAction` so a `route_to` field can be added later as an
additive `#[serde(default)]` field without a breaking schema change.

### FR-007: Recovery Fires on Terminal Abort-Class Failure

**WHEN** `propagate_failure()` (`crates/zeph-orchestration/src/dag.rs:223-322`) is invoked for a
task `T` **AND** `T`'s effective failure strategy is `FailureStrategy::Abort` (the default arm,
`dag.rs:243-253`) **OR** `FailureStrategy::Retry` with `retry_count >= max_retries` (the
retry-exhausted arm, `dag.rs:281-298`) **AND** `T.recovery.state_injection == Some(v)`,
**THE SYSTEM SHALL**, instead of the existing Abort-equivalent branch:

1. Set `T.status = TaskStatus::Completed`.
2. Set `T.result = Some(TaskResult { output: v, artifacts: vec![], duration_ms: 0, agent_id: None,
   agent_def: Some("__recovery__".to_string()) })`.
3. Leave `graph.status` unmodified (**not** set to `Failed`).

**WHEN** `T.recovery` is `None` **OR** `T.recovery.state_injection` is `None`, **THE SYSTEM
SHALL** preserve the exact existing Abort-equivalent behavior with zero change (BG-04).

### FR-008: Dependents Unblock Through the Existing Path — No New Consumption Machinery

**THE SYSTEM SHALL** rely entirely on existing mechanisms to deliver a recovered node's output to
its dependents: the `Pending`→`Ready` transition in `ready_tasks()`
(`crates/zeph-orchestration/src/dag.rs:179-208`, which unblocks a dependent once all
`depends_on` entries are `Completed`) and `build_task_prompt()`'s existing `Completed`-only
dependency filter plus SEC-ORCH-01 sanitizer
(`crates/zeph-orchestration/src/scheduler/router.rs:18-59`). **THE SYSTEM SHALL NOT** introduce
any new prompt-construction or consumption code path for recovered output.

### FR-009: Recovery Does Not Pause the Graph

**WHEN** a node recovers via FR-007, **THE SYSTEM SHALL NOT** set `graph.status =
GraphStatus::Paused` or `GraphStatus::Failed` as a side effect — independent, non-dependent
branches in the same graph **SHALL** continue executing unaffected, and the graph **SHALL** reach
its normal terminal status (`Completed`/`Failed`/`Paused`) based only on the remaining tasks'
outcomes.

### FR-010: Recovery-Completed Node Bypasses the Completion-Event Pipeline (Documented)

**THE SYSTEM SHALL** document that a node completed via FR-007 skips the normal
completion-event handler entirely (predicate verification, `token_budget_cents` check,
`verify_completeness`/`Verify` emission) because the transition happens synchronously inside
`propagate_failure()`, not through the event-driven completion path. **THE SYSTEM SHALL** rely on
FR-011's `validate()` guard to make this safe (a predicate-gated node is never recovery-eligible).

---

## 4. `validate()` Recovery Guards

### FR-011: Reject `recovery` + `verify_predicate` Co-Configuration

**WHEN** `validate()` (`crates/zeph-orchestration/src/dag.rs:37-91`) processes a `TaskNode` with
both `recovery.is_some()` **AND** `verify_predicate.is_some()`, **THE SYSTEM SHALL** return
`Err(OrchestrationError::InvalidGraph(...))` naming the offending task index.

> **Rationale:** recovery (FR-007) bypasses the completion-event handler where predicate
> verification runs (FR-010). A predicate-gated node must not be recovery-eligible, or an
> unverified synthetic output could reach downstream consumers as if it had passed verification.

### FR-012: Warn on `recovery` Under `Skip`/`Ask` Failure Strategy

**WHEN** `validate()` processes a `TaskNode` with `recovery.is_some()` **AND** its effective
failure strategy (own override or graph default) is `FailureStrategy::Skip` or
`FailureStrategy::Ask`, **THE SYSTEM SHALL** emit `tracing::warn!` naming the task and its
strategy, but **SHALL NOT** reject the graph.

> **Rationale:** recovery only fires from the `Abort`/retry-exhausted-`Retry` branches of
> `propagate_failure()` (FR-007) — under `Skip` or `Ask` it is configured but inert. This is a
> surfaced footgun, not an error: `Skip`/`Ask` semantics are explicit author choices and remain
> completely unchanged (BG-04).

---

## 5. Cascade-Abort vs. Recovery Precedence

### FR-013: Cascade-Abort Takes Precedence Over Recovery — No Code Reordering

**THE SYSTEM SHALL** rely on the existing event-path ordering in `handle_failed_outcome()`
(`crates/zeph-orchestration/src/scheduler/tick/mod.rs:590-681`): task set `Failed` (`:598`) →
`record_outcome(false)` (`:601-602`) → lineage build → fan-out cascade check that `return`s
`abort_dag_with_lineage(...)` early on trip (`:629-647`) → linear-chain cascade check that
similarly `return`s early (`:649-662`) → **only then** `propagate_failure()` (`:664`). **THE
SYSTEM SHALL NOT** reorder this sequence — recovery is structurally unreachable whenever a cascade
abort fires, because `propagate_failure()` (where FR-007 lives) is never reached on that path.

### FR-014: Document the Pre-Existing Timeout-vs-Event Recovery Asymmetry

**THE SYSTEM SHALL** document, as a single authoritative statement of "recovery on terminal
failure": recovery fires inside `propagate_failure()`; whether that call site is reached depends
on failure origin. The **timeout** path (`check_timeouts()` → `propagate_failure()`, no
`record_outcome`/cascade evaluation on that path today) **always** reaches `propagate_failure()`.
The **event** path (`handle_failed_outcome()`) reaches it **only if no cascade abort fired**
(FR-013). **THE SYSTEM SHALL** state this is a property of the pre-existing cascade design that
recovery inherits unchanged, not a new inconsistency introduced by this feature.

### FR-015: Document the Recorded-Then-Recovered Second-Order Effect (Accepted v1 Limitation)

**THE SYSTEM SHALL** document that `record_outcome(task_id, false, ...)` (`tick/mod.rs:601-602`)
runs **before** Mode-1 recovery can rescue the node on the event path, so a subsequently-recovered
failure still counts once in the cascade detector's history and could contribute to a later
cascade threshold trip. **THE SYSTEM SHALL** accept this as a documented v1 limitation — amending
the cascade record on successful recovery is an explicitly out-of-scope candidate follow-up
refinement (§8), not a v1 requirement.

---

## 6. Durability

### FR-016: No Resume Re-Scan — Same-Tick Snapshot Atomicity Is the Guarantee

**THE SYSTEM SHALL NOT** add any resume-time re-evaluation logic for pending or in-flight
recovery. **THE SYSTEM SHALL** rely on the verified existing invariant: the recovery mutation
inside `propagate_failure()` is synchronous (no `.await`), `propagate_failure()` runs inside the
fully-synchronous `scheduler.tick()` (`crates/zeph-core/src/agent/scheduler_loop.rs:338`), and
`save_graph_snapshot(...).await` runs later in the **same** loop iteration, gated on
`take_graph_dirty()` (`scheduler_loop.rs:547-551`, and `graph_dirty` is set at the start of
`handle_failed_outcome()`/`check_timeouts()`). A node's `Failed`→`Completed` recovery transition
therefore always lands in the **same** persisted snapshot as the triggering failure — there is no
crash window where a `Failed` status persists without its already-applied recovery, and no
crash window where a mid-tick crash leaves recovery "half-applied" (a mid-tick crash resumes from
the *prior* snapshot, where the task is still `Running`; the failure and recovery both re-fire
cleanly from that state on the next tick, per `crates/zeph-orchestration/src/scheduler/mod.rs:389-430`).

---

## 7. Configuration

### FR-017: Reuse `task_timeout_secs` as the Run-Timeout Global Default

**THE SYSTEM SHALL NOT** add a new config field for the run-timeout global default — the existing
`OrchestrationConfig.task_timeout_secs` (`crates/zeph-config/src/experiment.rs:274`, default
300s) continues to serve as the fallback whenever a `TaskNode` sets no `timeout.run_timeout_secs`
override (FR-002).

### FR-018: New `default_idle_timeout_secs` Config Field

**THE SYSTEM SHALL** add exactly one new field, `default_idle_timeout_secs: Option<u64>`
(`None` = off, matching the field's v1 no-op status), to `OrchestrationConfig`
(`crates/zeph-config/src/experiment.rs`). **THE SYSTEM SHALL** provide the full mandatory
integration set for this field per this project's Development Rules:

1. `config.toml` `[orchestration]` section entry, documented as reserved/not-yet-enforced (FR-005).
2. `--init` wizard entry in `step_orchestration()` (`src/init/agents.rs:11`), same reserved-field
   wording.
3. A `--migrate-config` step in the `MIGRATIONS` registry
   (`crates/zeph-config/src/migrate/mod.rs:646-`) that adds the field with a `None` default for
   pre-existing configs.
4. `#[serde(default)]` on the field for forward compatibility with configs persisted before this
   feature existed.

**THE SYSTEM SHALL NOT** add a CLI subcommand or TUI command-palette entry for this field — it is
a passive config default, not an imperative action, consistent with how `task_timeout_secs` is
exposed today (config-only, no dedicated CLI/TUI surface).

### FR-019: Per-Task Fields Are Graph Data, Not Config — No Wizard/Migration Surface

**THE SYSTEM SHALL** treat `TaskNode.timeout` and `TaskNode.recovery` as planner-authored graph
data, following the existing precedent set by `failure_strategy`/`max_retries` overrides on the
same struct. **THE SYSTEM SHALL NOT** add `--init`/`--migrate-config` entries for these two
fields — **THE SYSTEM SHALL** rely solely on `#[serde(default, skip_serializing_if =
"Option::is_none")]` (FR-001, FR-006) for forward compatibility with graphs persisted before this
feature existed.

---

## 8. Documentation and Metrics

### FR-020: Annotate the `ready_tasks()` `Ready`-Arm Dependency Bypass as Load-Bearing

**THE SYSTEM SHALL** add a doc comment to the `Ready` arm of `ready_tasks()`
(`crates/zeph-orchestration/src/dag.rs:185-191`, which checks only predicate clearance, **not**
`depends_on` completion) stating that this bypass is load-bearing for Mode-1 recovery's unblock
path — a recovered node's dependents transition through the `Pending` arm (which does check
`depends_on` completion) using the recovered node's now-`Completed` status, but a future
refactor that "fixes" the `Ready` arm to also re-check `depends_on` could change dispatch
semantics for predicate-gated tasks in ways that interact with recovery. **THE SYSTEM SHALL**
also note that this same bypass is why Mode 2 (deferred, §9) cannot be made safe by a
`depends_on`-based topology constraint alone.

### FR-021: Recovered-Node Metrics Classification — Resolved, Status-Derived

**THE SYSTEM SHALL** document, as the resolution to critic finding M-a, that a Mode-1-recovered
node requires **no special-case metrics handling**: `OrchestrationMetrics.tasks_completed`/
`tasks_failed` (`crates/zeph-core/src/metrics.rs:101-107`) are populated exclusively by
`finalize_plan_completed()`/`finalize_plan_failed()`
(`crates/zeph-core/src/agent/plan.rs:722-833`), which filter `completed_graph.tasks` by **final**
`TaskStatus` at graph-finalization time — not incremented at failure-event time. Because a
recovered node's status is `Completed` (FR-007) by the time the graph reaches a terminal state,
it is counted in `tasks_completed` and never in `tasks_failed`, with zero code change required.

---

## 9. Deferred Requirements (Acknowledged)

### FR-D-01: Mode 2 (`route_to` Reroute-to-Alternate-Node Recovery)

Deferred (BRD §5). Blocked by three findings from the design review:

- **N5 (root cause):** a `depends_on == [failed_task]` fallback-node design dispatches on the
  failed task's *success* (`ready_tasks()`'s `Pending` arm unblocks on `Completed`,
  `dag.rs:192-203`), not its failure — inverted from the intended fallback semantics. Not fixable
  by any dependency-topology constraint; requires a new `TaskStatus::Dormant` marker (excluded
  from `ready_tasks()` dispatch, activated only by explicit recovery) or an explicit on-failure
  edge concept distinct from `depends_on`.
- **N1:** reusing the existing Skip-BFS (`dag.rs:262-276`) to revive a fallback node would still
  leave that node's own downstream subtree `Skipped` permanently — the BFS would need to exclude
  the recovery target and its transitive closure.
- **N3:** `build_task_prompt()`'s `Completed`-only dependency filter
  (`crates/zeph-orchestration/src/scheduler/router.rs:23-34`) silently drops a `Failed` source
  task's `state_injection` — a rerouted fallback would dispatch with zero context from the failed
  task, needing a targeted extension routed through the SEC-ORCH-01 sanitizer.

If Mode 2 is redesigned with a real `Dormant`/on-failure-edge mechanism, the runtime status guard
on the reroute target (checking it is not unexpectedly `Running`/`Completed` at recovery time)
becomes load-bearing, not merely defensive, because a proper on-failure-edge fallback could
legitimately be reachable via other paths too.

### FR-D-02: Idle-Timeout Progress-Signal Plumbing (Alt A)

Deferred (BRD §5). Target design: a coalescing per-task `Arc<AtomicU64>`/`watch` progress
timestamp — explicitly **not** a second queued channel, since the existing completion-event
`mpsc::channel(64)` (`crates/zeph-orchestration/src/scheduler/mod.rs:518`) already drops events
under saturation, and multiplexing progress signals onto it risks evicting real completion
events. Requires cross-crate wiring: the `zeph-subagent` spawn path and the `zeph-core`
`RunInline` loop would both need to emit progress signals. Not built in v1; FR-005 ships the field
as a documented no-op instead.

---

## 10. Traceability Matrix

| Requirement | BRD Goal | Architect/Critic Source |
|-------------|----------|--------------------------|
| FR-001..FR-004 | BG-01, BG-04 | Architect base plan + v3 acceptance criterion 1 |
| FR-005 | BG-03 | Architect v3 decision (2); critic finding M-b |
| FR-006..FR-010 | BG-02, BG-04 | Architect v3 decision (3); critic re-confirmation of Mode-1 soundness |
| FR-011, FR-012 | BG-02, BG-05 | Architect v3 decision (4); critic re-confirmation ("correct and sufficient now that route_to is gone") |
| FR-013..FR-015 | BG-04 | Architect N2 (round 2, carried to v3); critic code-verified re-confirmation |
| FR-016 | BG-04 | Architect M2 correction (v3, supersedes v2's proposed resume re-scan); critic independent re-verification |
| FR-017..FR-019 | BG-01, BG-04 | Architect "Carried over unchanged (critic-endorsed)" section |
| FR-020 | BG-05 | Architect v3 acceptance criterion 7 |
| FR-021 | BG-04 | Critic finding M-a; resolved during spec formalization against `plan.rs:722-833` |
| FR-D-01 | (deferred) | Architect N5/N1/N3 retraction and deferral; critic "RESOLVED by deferral" |
| FR-D-02 | (deferred) | Architect v3 decision (2), "Carried to follow-up" |
