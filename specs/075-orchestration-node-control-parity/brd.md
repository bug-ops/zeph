---
aliases:
  - Orchestration Node Control Parity BRD
  - Node Timeout / Retry-Exhausted Recovery BRD
  - BRD 6021
tags:
  - sdd
  - brd
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/075-orchestration-node-control-parity/spec]]"
  - "[[specs/075-orchestration-node-control-parity/srs]]"
  - "[[specs/075-orchestration-node-control-parity/nfr]]"
  - "[[009-orchestration/spec]]"
---

# BRD: Orchestration Node Control Parity — Per-Task Timeouts and Retry-Exhausted Recovery (GitHub #6021)

## 1. Business Context

`zeph-orchestration`'s `TaskGraph`/`DagScheduler` is this project's designated architectural
comparator to LangGraph (LangChain, Python) for the durable, checkpointed-DAG-execution
dimension — no other tracked reference agent covers it
(`.local/testing/playbooks/competitive-parity.md`). A prior research spec
(`.local/specs/059-orchestration-node-control-parity/spec.md`, 2026-07-11) identified two
LangGraph `add_node` capabilities Zeph lacks: per-node `TimeoutPolicy` (hard wall-clock vs.
idle/no-progress caps) and a node-level error handler that can recover a task after retries are
exhausted instead of collapsing straight to abort. This BRD formalizes the v1 slice of that gap
that a three-round architect/critic design review (final verdict: **minor / approved**,
`.local/handoff/2026-07-13T21-16-30-critic.md`) confirmed is implementation-ready.

The review process itself materially narrowed scope: the initial design included a `route_to`
reroute-to-fallback-node recovery mode. Round-3 critique (`N5`) proved that mode's proposed safety
mechanism was **inverted** — a `depends_on == [failed_task]` fallback dispatches on the failed
task's *success*, not its failure, which is the exact opposite of a fallback. The architect
retracted that reasoning and dropped the reroute mode from v1 entirely (see §5, §8).

## 2. Problem Statement

Two gaps, both already documented by the prior research spec and re-confirmed against current
code (HEAD `d93d82e8`):

1. **Timeout is a single global `Duration`, not a per-task override.**
   `OrchestrationConfig.task_timeout_secs` (`crates/zeph-config/src/experiment.rs:274`, default
   300s) feeds exactly one `task_timeout: Duration` field on the scheduler, applied uniformly by
   `check_timeouts()` (`crates/zeph-orchestration/src/scheduler/tick/mod.rs:727-768`) and by
   `wait_event()`'s nearest-deadline computation (`tick/mod.rs:261-270`). `TaskNode` already
   supports per-task overrides for `failure_strategy`, `max_retries`, and `token_budget_cents`
   (`crates/zeph-orchestration/src/graph.rs:379-452`) — timeout is the one override the crate's
   own established pattern is missing.
2. **Retry-exhausted failure always collapses to Abort-equivalent termination.**
   `propagate_failure()` (`crates/zeph-orchestration/src/dag.rs:223-322`) implements
   `FailureStrategy::Retry` by incrementing `retry_count` until `max_retries`, then falls through
   to the same branch as `Abort` (`dag.rs:281-298`, comment: "Retry exhausted — treat as Abort").
   The only non-abort escape hatch, `FailureStrategy::Ask`, pauses the **entire graph**
   (`GraphStatus::Paused`) for human intervention — there is no autonomous, programmatic recovery
   path.

## 3. Business Goals

| ID | Goal | Priority |
|----|------|----------|
| BG-01 | An operator can override the graph-global task timeout on individual tasks, so heterogeneous DAG workloads (a fast classification subtask alongside a multi-minute code-generation subtask) do not share one ill-fitting timeout value | P1 |
| BG-02 | A task author can configure a node to substitute a synthetic output and continue (instead of aborting or pausing the whole graph) when that node's terminal failure is an `Abort`-default or retry-exhausted `Retry` outcome | P1 |
| BG-03 | The idle/no-progress timeout concept is defined and config-surfaced now (so the schema does not need a breaking change later) without pretending to be enforced before the progress-signal plumbing it depends on exists | P2 |
| BG-04 | Every new capability degrades to today's exact existing behavior when not configured — zero regression for graphs that opt into neither feature | P1 |
| BG-05 | The capability set is scoped to what the design review proved safe for v1: recovery is a single declarative "substitute output, keep going" mode, not a reroute-to-alternate-node mode (that mode is deferred pending a redesign — see §5) | P1 |

## 4. Stakeholders

| Role | Interest |
|------|----------|
| Operator running long DAG workflows with heterogeneous task durations | Wants per-task timeout control instead of one global value that is either too loose or too tight |
| Task/plan author designing a DAG with a legitimately-flaky node (e.g. a data-fetch task) | Wants an autonomous fallback path instead of a hard abort or a graph-wide pause on every transient exhaustion |
| Zeph maintainers | Want a minimal, spec-039-compliant, additive extension — no new crate, no new `tokio::spawn` site, zero behavior change for existing graphs |
| Future implementation session (`/rust-team` per this project's constraint that CI/spec sessions do not write source code) | Inherits a fully traceable, code-cited contract with no open architectural questions — the three-round review already resolved them |
| Future follow-up spec authors (Mode 2 redesign, Alt A idle-progress plumbing) | Inherit a precise, code-grounded problem statement for why those items were deferred, not just a "TODO" |

## 5. Out of Scope

| Item | Reason |
|------|--------|
| Mode 2 (`route_to` reroute-to-alternate-node recovery) | **N5 (root cause):** a naive `depends_on == [failed_task]` fallback-node design dispatches when the failed task *succeeds* (`ready_tasks()`'s `Pending` arm unblocks on `Completed`, `dag.rs:192-203`), not when it fails — the exact opposite of a fallback. No dependency-topology constraint can fix this; it requires a genuinely new mechanism (a `TaskStatus::Dormant` marker or an explicit on-failure edge). **N1:** reusing the existing Skip-BFS (`dag.rs:254-280`) to un-stick a revived fallback node would still leave that node's own downstream subtree permanently `Skipped`. **N3:** `build_task_prompt`'s `Completed`-only dependency filter (`crates/zeph-orchestration/src/scheduler/router.rs:23-34`) would silently drop a `Failed` source task's `state_injection`, so a rerouted fallback would receive zero context. All three require a real redesign, not a v1 fix — deferred to a follow-up issue |
| Idle-timeout progress-signal plumbing (Alt A: coalescing per-task `Arc<AtomicU64>`/`watch` progress timestamp) | No heartbeat/liveness/progress-signal mechanism exists anywhere in `zeph-subagent` or `zeph-orchestration` today (verified: zero hits). Building it is a distinct, cross-crate wiring effort (spawn path + `RunInline` loop instrumentation) — v1 defines the field and its target semantics but ships it as a documented no-op |
| Any change to `FailureStrategy::Abort`/`Skip`/`Ask` semantics for tasks that configure neither `timeout` nor `recovery` | Existing behavior for graphs that never opt in must be bit-for-bit unchanged (BG-04) |
| A resume-time re-scan for in-flight recovery | Verified unnecessary: the recovery mutation is synchronous (no `.await`) inside `propagate_failure()`, which runs inside the fully-synchronous `scheduler.tick()` (`crates/zeph-core/src/agent/scheduler_loop.rs:338`); `save_graph_snapshot()` runs later in the **same** loop iteration, gated on `take_graph_dirty()` (`scheduler_loop.rs:547-551`). A failure and its Mode-1 recovery always land in the same snapshot — no crash window exists where one persists without the other |
| Any change to the cascade-abort event-path ordering | The existing ordering (`handle_failed_outcome`, `crates/zeph-orchestration/src/scheduler/tick/mod.rs:590-681`: `Failed` → `record_outcome` → cascade checks that `return` early → `propagate_failure`) already makes cascade-abort take precedence over recovery with zero code reordering required |
| Cross-graph or cross-session recovery routing | Scope is intra-graph only |

These deferrals are carried into `srs.md` as acknowledged-deferred requirements (FR-D-01, FR-D-02).

## 6. Success Criteria

| ID | Criterion | Measurable |
|----|-----------|-----------|
| SC-01 | Graphs that configure neither `timeout` nor `recovery` on any `TaskNode` behave identically to current behavior | Regression test suite covering `check_timeouts()`, `wait_event()`, and `propagate_failure()` passes unchanged |
| SC-02 | A per-task `run_timeout_secs` override supersedes the graph-global `task_timeout` for the task that declares it, for both spawned and `RunInline` tasks | Test: a short per-task override fires before the (longer) global default would have, on both dispatch paths |
| SC-03 | `idle_timeout_secs` is defined, serializable, and config-surfaced, but never fires in v1 | Test: a task configured with a short `idle_timeout_secs` and long-running (idle) execution is NOT flagged as timed out by that field; `--init` wizard text and config.toml comment both state "reserved — not yet enforced" |
| SC-04 | A node with `state_injection` configured, on `Abort`-default or retry-exhausted `Retry` terminal failure, transitions to `Completed` with the synthetic output, and dependents unblock and consume that output through the existing sanitizer | Test: single failing node with `state_injection` set — dependents receive the injected value through `build_task_prompt`, `graph.status` remains `Running` |
| SC-05 | A cascade-abort (fan-out or linear-chain) always takes precedence over recovery — recovery never fires once a cascade abort has triggered for the same event | Test: a node with `recovery` configured whose failure also trips the cascade-abort threshold ends the graph `Failed`, not recovered |
| SC-06 | `validate()` rejects a node that sets both `recovery` and `verify_predicate`, and warns (not rejects) when `recovery` is configured under `Skip`/`Ask` | Config/graph-construction validation tests for both cases |
| SC-07 | A recovered node is counted as `tasks_completed` (never `tasks_failed`) in `OrchestrationMetrics`, with no special-casing required | Verified against the existing status-derived counting in `finalize_plan_completed`/`finalize_plan_failed` (`crates/zeph-core/src/agent/plan.rs:722-833`) |
| SC-08 | The new `default_idle_timeout_secs` config field ships with full config.toml / `--init` / `--migrate-config` integration per this project's mandatory integration-point rule | `--init` wizard prompt, `--migrate-config` step, and config.toml documentation all present |

## 7. Constraints

- No new crate; all new code lands in `zeph-orchestration` (data model + scheduling logic) and
  `zeph-config` (one new config field + validation + migration).
- Zero new `tokio::spawn()` call sites — recovery is a synchronous data mutation inside an
  already-synchronous `tick()`; no async work, so no `*_provider` field is needed either (per this
  project's multi-model design principle, which only applies to subsystems that call an LLM —
  recovery does not).
- `TaskNode`'s new `timeout`/`recovery` fields follow the crate's existing
  `Option<T>`-with-graph-default override pattern (`failure_strategy`, `max_retries`,
  `token_budget_cents`) and its existing `#[serde(default, skip_serializing_if = "Option::is_none")]`
  forward-compatibility convention (`network_scope`, `asset_sensitivity`,
  `crates/zeph-orchestration/src/graph.rs:441-451`).
- No code reordering in the existing cascade-abort event path — recovery slots into the existing
  `propagate_failure()` call site unchanged.

## 8. Dependencies

| Dependency | Type | Notes |
|------------|------|-------|
| `TaskNode` (`crates/zeph-orchestration/src/graph.rs:379-452`) | Internal | New `timeout: Option<TimeoutPolicy>` and `recovery: Option<RecoveryAction>` fields, following the existing per-task override precedent |
| `check_timeouts()` / `wait_event()` (`crates/zeph-orchestration/src/scheduler/tick/mod.rs:727-768`, `:254-270`) | Internal | Extended to compute a per-task effective run-timeout instead of the single global `task_timeout` |
| `RunInline` inline `tokio::select!` (`crates/zeph-core/src/agent/scheduler_loop.rs:258-`) | Internal | The only structurally viable enforcement site for `RunInline` tasks, since the tick loop is blocked for the task's whole duration |
| `propagate_failure()` (`crates/zeph-orchestration/src/dag.rs:223-322`) | Internal | Gains the Mode-1 recovery branch on terminal `Abort`/retry-exhausted `Retry` |
| `validate()` (`crates/zeph-orchestration/src/dag.rs:37-91`) | Internal | Gains the recovery/predicate reject guard and the recovery/Skip-Ask warn guard |
| `handle_failed_outcome()` cascade-abort event path (`crates/zeph-orchestration/src/scheduler/tick/mod.rs:590-681`) | Internal | Read-only dependency — its existing ordering is the precedence mechanism; unchanged |
| `build_task_prompt()` (`crates/zeph-orchestration/src/scheduler/router.rs:18-59`) | Internal | Existing `Completed`-only dependency filter + SEC-ORCH-01 sanitizer; unmodified, consumes the synthetic recovery output as-is |
| `OrchestrationConfig` (`crates/zeph-config/src/experiment.rs:261-`) | Internal | New `default_idle_timeout_secs: Option<u64>` field; reuses existing `task_timeout_secs` as the run-timeout global default |
| `MIGRATIONS` registry (`crates/zeph-config/src/migrate/mod.rs:646-`) | Internal | New migration step adds the field with a `None` default |
| `step_orchestration()` (`src/init/agents.rs:11`) | Internal | `--init` wizard integration point |
| `finalize_plan_completed`/`finalize_plan_failed` (`crates/zeph-core/src/agent/plan.rs:722-833`) | Internal | Read-only confirmation that metrics are status-derived at graph finalization, not event-incremented — resolves the recovered-node metrics question with no code change needed |
