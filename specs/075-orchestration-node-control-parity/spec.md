---
aliases:
  - Orchestration Node Control Parity
  - Node Timeout / Retry-Exhausted Recovery
  - Spec 6021
tags:
  - sdd
  - spec
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[specs/075-orchestration-node-control-parity/brd]]"
  - "[[specs/075-orchestration-node-control-parity/srs]]"
  - "[[specs/075-orchestration-node-control-parity/nfr]]"
  - "[[specs/075-orchestration-node-control-parity/plan]]"
  - "[[001-system-invariants/spec]]"
  - "[[009-orchestration/spec]]"
  - "[[039-background-task-supervisor/spec]]"
issues:
  - "#6021"
---

# Spec: Orchestration Node Control Parity — Per-Task Timeouts and Retry-Exhausted Recovery Routing (GitHub #6021)

> [!info]
> **Status update (#6245):** `idle_timeout_secs` moved from documented no-op to enforced —
> see the Alt-A progress-signal plumbing design in `.local/handoff/2026-07-16T21-37-11-architect.md`
> (+ amendment `2026-07-16T21-53-03-architect.md`). FR-005 below and the "documented no-op"
> language throughout this spec describe the v1 state this spec originally shipped; treat
> them as historical for `idle_timeout_secs` specifically — run-timeout behavior is unchanged.
>
> `TaskNode` gains an optional per-task `TimeoutPolicy` (hard `run_timeout_secs` override,
> enforced; `idle_timeout_secs`, defined but a documented no-op in v1) and an optional
> `RecoveryAction` (`state_injection` — substitute a synthetic output and continue, on terminal
> `Abort`-default or retry-exhausted `Retry` failure). No new crate, no new `tokio::spawn` site,
> zero behavior change for graphs that configure neither field. This spec is the authoritative
> implementation contract, derived from a three-round architect/critic design review (final
> critic verdict: **minor / approved**, `.local/handoff/2026-07-13T21-16-30-critic.md`). It
> formalizes that design into traceable requirements; it does not re-derive the architecture.
> Supersedes `.local/specs/059-orchestration-node-control-parity/spec.md` (2026-07-11 research
> draft), whose FR-001..FR-010/NFR-001..NFR-006/SC-001..SC-004 baseline is carried forward and
> narrowed to what the design review proved safe for v1 — most significantly, the research
> draft's `route_to` reroute mode is dropped from v1 entirely (§7).

## Sources

### External
- LangGraph (LangChain, Python) v1.2.x, current 1.2.9 (2026-07-10) — `TimeoutPolicy(run_timeout=...,
  idle_timeout=...)` on `add_node`; node-level error handler returning a `Command` after retry
  exhaustion. Source material for the original parity finding.

### Internal
| File | Contents |
|---|---|
| `crates/zeph-orchestration/src/graph.rs:379-452` | `TaskNode` struct; existing per-task override precedent (`failure_strategy`, `max_retries`, `token_budget_cents`) and existing `#[serde(default, skip_serializing_if = "Option::is_none")]` forward-compat pattern (`network_scope`, `asset_sensitivity`) — the new `timeout`/`recovery` fields follow both |
| `crates/zeph-orchestration/src/dag.rs:37-91` | `validate()` — structural DAG validation; new recovery guards join the existing per-task loop at `:51-77` |
| `crates/zeph-orchestration/src/dag.rs:179-208` | `ready_tasks()` — `Ready` arm (`:185-191`, predicate-only, no `depends_on` re-check) and `Pending` arm (`:192-203`, `depends_on` completion check) — the `Pending` arm is how a recovered node's dependents unblock |
| `crates/zeph-orchestration/src/dag.rs:223-322` | `propagate_failure()` — `Abort` arm (`:243-253`), `Skip` arm (`:254-280`), `Retry` arm with retry-exhausted fallthrough (`:281-298`), `Ask` arm (`:299-303`), non-exhaustive wildcard arm (`:308-320`, dead code today — no other `FailureStrategy` variant exists, logs+defaults to Abort-equivalent for a future variant) — the new recovery branch attaches to the `Abort` arm and the retry-exhausted fallthrough only |
| `crates/zeph-orchestration/src/scheduler/tick/mod.rs:590-681` | `handle_failed_outcome()` — event-path failure handling: `Failed` status set (`:599`), `record_outcome` (`:601-602`), fan-out cascade check (`:629-647`), linear-chain cascade check (`:649-662`), `propagate_failure()` call (`:664`) — the existing ordering is the cascade-vs-recovery precedence mechanism |
| `crates/zeph-orchestration/src/scheduler/tick/mod.rs:690-` | `abort_dag_with_lineage()` — sets `graph.status = Failed` unconditionally; called by both cascade checks before `propagate_failure()` is reached |
| `crates/zeph-orchestration/src/scheduler/tick/mod.rs:727-768` | `check_timeouts()` — per-running-task timeout evaluation; gains per-task effective-timeout lookup |
| `crates/zeph-orchestration/src/scheduler/tick/mod.rs:254-270` | `wait_event()` — nearest-timeout-deadline computation (`:261-270`), currently uniform on `self.task_timeout`; becomes per-task-aware |
| `crates/zeph-orchestration/src/scheduler/router.rs:18-59` | `build_task_prompt()` — `Completed`-only dependency filter (`:23-34`), SEC-ORCH-01 sanitizer (`:59`) — unmodified; consumes recovered `state_injection` output as-is |
| `crates/zeph-core/src/agent/scheduler_loop.rs:258-` | `RunInline` inline `tokio::select!` — gains a third `tokio::time::timeout` branch |
| `crates/zeph-core/src/agent/scheduler_loop.rs:338,547-551` | `scheduler.tick()` call site and `save_graph_snapshot()` gating on `take_graph_dirty()` — the same-tick snapshot atomicity durability guarantee |
| `crates/zeph-orchestration/src/scheduler/mod.rs:389-430` | `resume_from()` — rebuilds the `running` map from persisted `TaskStatus::Running` entries; unmodified by this feature (no resume re-scan added) |
| `crates/zeph-orchestration/src/scheduler/mod.rs:518` | Completion-event `mpsc::channel(64)` — cited as the reason Alt A (deferred) must not reuse this channel for progress signals |
| `crates/zeph-config/src/experiment.rs:261-,274` | `OrchestrationConfig`, existing `task_timeout_secs` (reused as the run-timeout global default, no new field) |
| `crates/zeph-config/src/migrate/mod.rs:646-` | `MIGRATIONS` registry — new step for `default_idle_timeout_secs` |
| `src/init/agents.rs:11` | `step_orchestration()` — `--init` wizard integration point |
| `crates/zeph-core/src/metrics.rs:101-107` | `OrchestrationMetrics` — `tasks_completed`/`tasks_failed` |
| `crates/zeph-core/src/agent/plan.rs:722-833` | `finalize_plan_completed()`/`finalize_plan_failed()` — confirms metrics are status-derived at graph-finalization time, resolving the recovered-node metrics classification with no code change (critic finding M-a) |

---

## 1. Overview

### Problem Statement

`zeph-orchestration`'s `TaskGraph`/`DagScheduler` controls execution timing and terminal-failure
handling more coarsely than LangGraph's `add_node` API surface: timeout is a single global
`Duration` with no per-task override (inconsistent with the crate's own established override
pattern for `failure_strategy`/`max_retries`/`token_budget_cents`), and retry-exhausted failure
always collapses to Abort-equivalent termination, with the only non-abort escape hatch
(`FailureStrategy::Ask`) pausing the entire graph rather than allowing autonomous recovery. Full
problem framing: `[[specs/075-orchestration-node-control-parity/brd]]` §1-2.

### Goal

A `TaskNode` can declare a per-task run-timeout override enforced on both spawned and `RunInline`
tasks, and can declare a Mode-1 recovery action that substitutes a synthetic output and lets the
graph continue past a terminal `Abort`-default or retry-exhausted `Retry` failure — without
pausing unrelated concurrent work. Both are additive, `Option`-typed, and produce zero behavior
change for any graph that does not opt in.

### Out of Scope

See `[[specs/075-orchestration-node-control-parity/brd]]` §5 for the full list with rationale.
Summary: Mode 2 (`route_to` reroute-to-alternate-node recovery, blocked by findings N5/N1/N3 —
requires a `TaskStatus::Dormant`/on-failure-edge redesign), idle-timeout progress-signal plumbing
(Alt A), any change to default `Abort`/`Skip`/`Ask` semantics, and a resume-time re-scan (proven
unnecessary by same-tick snapshot atomicity).

Full requirement-level detail: `[[specs/075-orchestration-node-control-parity/srs]]`. Quality
targets: `[[specs/075-orchestration-node-control-parity/nfr]]`.

---

## 2. Functional Requirements

See `[[specs/075-orchestration-node-control-parity/srs]]` for the complete EARS-notation requirement
set (FR-001 through FR-021, plus FR-D-01/FR-D-02 deferred) and traceability matrix. Summary:

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001..004 | `TimeoutPolicy` data model; per-task run-timeout enforcement on both spawned and `RunInline` dispatch; `wait_event()` made per-task-aware | must |
| FR-005 | `idle_timeout_secs` defined/config-surfaced, documented no-op, loudly marked reserved | must |
| FR-006..010 | `RecoveryAction` data model (`state_injection` only, `route_to` deferred); Mode-1 recovery in `propagate_failure()`; existing dependent-unblock/prompt-consumption path reused unmodified; no graph pause | must |
| FR-011, FR-012 | `validate()` reject (recovery + verify_predicate) and warn (recovery + Skip/Ask) guards | must |
| FR-013..015 | Cascade-abort precedence via existing ordering; documented timeout-vs-event asymmetry; documented recorded-then-recovered limitation | must |
| FR-016 | Durability: no resume re-scan, same-tick snapshot atomicity is the guarantee | must |
| FR-017..019 | Config: reuse `task_timeout_secs`; new `default_idle_timeout_secs` with full integration; per-task fields need only `#[serde(default)]` | must |
| FR-020 | Doc annotation: `ready_tasks()` `Ready`-arm bypass is load-bearing for recovery | must |
| FR-021 | Recovered-node metrics classification resolved (status-derived, no special case) | must |

---

## 3. Architecture / Design

### 3.1 Data Model

```rust
/// Per-task timeout override, mirroring LangGraph's `TimeoutPolicy`.
/// `None` on either field falls back to the graph-global default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    /// Hard wall-clock cap. `None` falls back to `OrchestrationConfig.task_timeout_secs`.
    pub run_timeout_secs: Option<u64>,
    /// Idle/no-progress cap. Defined and config-surfaced but NOT enforced in v1 — see FR-005.
    pub idle_timeout_secs: Option<u64>,
}

/// Declarative recovery action for a node's terminal failure.
/// v1 supports Mode 1 only; `route_to` (Mode 2) is deferred and added later,
/// additively, as a further `#[serde(default)]` field on this same struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryAction {
    /// Substitute output injected as this node's `TaskResult.output` on recovery.
    pub state_injection: Option<String>,
}
```

Both attach to `TaskNode` (`graph.rs:379-452`) as:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub timeout: Option<TimeoutPolicy>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub recovery: Option<RecoveryAction>,
```

### 3.2 Timeout Enforcement — Two Dispatch Kinds, Two Enforcement Sites

```
TaskNode.timeout.run_timeout_secs
        │
        ├── Spawned task ──> check_timeouts() (tick/mod.rs:727)
        │                    effective = timeout.and_then(|t| t.run_timeout_secs)
        │                                .map(Duration::from_secs)
        │                                .unwrap_or(self.task_timeout)
        │                    wait_event() nearest-deadline (tick/mod.rs:261) uses the same
        │                    per-task effective value instead of the uniform self.task_timeout
        │
        └── RunInline task ──> scheduler_loop.rs:258 inline tokio::select! gains a third branch:
                               tokio::time::timeout(effective, run_inline_tool_loop(...))
                               (check_timeouts() cannot fire — the tick loop is blocked for the
                               task's whole duration on this path)
```

Both enforcement sites converge on the same `TaskOutcome::Failed`/`TaskStatus::Failed` shape, so
downstream failure handling (§3.3) is uniform regardless of which dispatch kind timed out.

### 3.3 Recovery — Mode 1 (`state_injection`)

```
propagate_failure(graph, failed_id, rev_adj)   [dag.rs:223]
        │
        ├── strategy == Abort ─────────────────┐
        │                                       │
        └── strategy == Retry, retries exhausted┤
                                                 ▼
                                    node.recovery?.state_injection == Some(v) ?
                                        │Yes                           │No
                                        ▼                              ▼
                            node.status = Completed        existing Abort-equivalent
                            node.result = Some(TaskResult{  branch (graph.status = Failed,
                              output: v, artifacts: [],      cancel Running tasks) — UNCHANGED
                              duration_ms: 0, agent_id: None,
                              agent_def: Some("__recovery__")
                            })
                            graph.status: UNCHANGED (stays Running)
                                        │
                                        ▼
                            next tick: ready_tasks() Pending arm sees dependents'
                            depends_on now Completed → Ready → dispatched
                                        │
                                        ▼
                            build_task_prompt() Completed-only filter (scheduler/router.rs:28)
                            picks up the recovered node's result; SEC-ORCH-01 sanitizer (:59)
                            applies exactly as it would to any normal completion
```

No new consumption machinery: the recovered node looks, to every downstream consumer, like a
task that completed normally with an unusual `agent_def` marker.

### 3.4 Precedence: Cascade-Abort Over Recovery (No Code Reordering)

```
handle_failed_outcome(task_id, error)   [tick/mod.rs:590]
        │
        ├── graph.tasks[task_id].status = Failed        [:599]
        ├── cascade_detector.record_outcome(false)       [:601-602]
        ├── build lineage chain
        ├── fan-out cascade check:
        │      trips? ──Yes──> return abort_dag_with_lineage(...)  [:629-647] ── recovery
        │      │No                                                              UNREACHABLE
        ├── linear-chain cascade check:
        │      trips? ──Yes──> return abort_dag_with_lineage(...)  [:649-662] ── recovery
        │      │No                                                              UNREACHABLE
        ▼
    propagate_failure(...)   [:664]  ── recovery (§3.3) can fire HERE, only if no cascade tripped

check_timeouts()   [tick/mod.rs:727]  ── NO record_outcome, NO cascade evaluation on this path
        │
        ▼
    propagate_failure(...)   [:749]  ── recovery ALWAYS reachable on the timeout path
```

This asymmetry (event-path recovery is conditional on no cascade trip; timeout-path recovery is
unconditional) is a property of the pre-existing cascade design — the cascade detector was never
fed by the timeout path — and recovery inherits it unchanged. No code reordering is required or
performed.

---

## 4. Key Invariants

### Always (without asking)

- A `TaskNode` with `timeout == None` and `recovery == None` behaves identically to current
  behavior — global `task_timeout` applies, `Abort`/retry-exhausted-`Retry` always falls through
  to Abort-equivalent termination, `Skip`/`Ask` are untouched (NFR-CO-01, NFR-CO-02).
- Both new `TaskNode` fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`,
  matching the existing `network_scope`/`asset_sensitivity` forward-compat precedent.
- Recovery mutates `node.status`/`node.result` synchronously, inside `propagate_failure()`, with
  no `.await` — this is what makes the same-tick snapshot atomicity durability guarantee hold
  (FR-016).
- `graph.status` is left unmodified by a Mode-1 recovery — independent branches always continue
  (FR-009).
- The existing cascade-check-before-`propagate_failure()` ordering in `handle_failed_outcome()`
  is preserved exactly — recovery attaches only inside `propagate_failure()`, never before it.
- `validate()` rejects `recovery.is_some() && verify_predicate.is_some()` on the same node
  (FR-011) and warns (does not reject) when `recovery.is_some()` under `Skip`/`Ask` (FR-012).
- The `idle_timeout_secs` field (per-task and global) is documented "reserved — not yet
  enforced" everywhere an operator can set it: `--init` wizard text and `config.toml` comment
  (FR-005, FR-018, critic finding M-b).
- New tracing spans on the recovery-application and per-task-timeout-cancellation code paths
  follow the `<crate_short>.<subsystem>.<operation>` naming convention (NFR-OB-02).

### Ask First

- Whether the recovery-application tracing span name is `orchestration.dag.recover_task` or a
  different name — cosmetic naming choice left to the implementing session, but must follow the
  project convention.
- Whether `default_idle_timeout_secs` gets a dedicated CLI/TUI surface beyond config.toml/`--init`
  — the existing `task_timeout_secs` precedent has none, and FR-018 does not require one, but this
  is worth confirming against current TUI settings-editor conventions (`[[061-tui-settings-editor-parity/spec]]`
  if it exists) before implementation.

### Never

- **NEVER** add a `route_to` field to `RecoveryAction` in this v1 implementation — Mode 2 is
  proven unsafe as originally designed (N5) and requires a `TaskStatus::Dormant`/on-failure-edge
  redesign that is explicitly out of scope here (FR-D-01).
- **NEVER** reorder the cascade-check-before-`propagate_failure()` sequence in
  `handle_failed_outcome()` — the existing ordering IS the cascade-over-recovery precedence
  mechanism (FR-013); reordering it is a correctness change, not a cleanup.
- **NEVER** add resume-time re-evaluation logic for recovery — same-tick snapshot atomicity
  already closes the crash window; adding a re-scan reintroduces exactly the idempotency risk the
  architect identified and retracted in round 2 (FR-016).
- **NEVER** let a predicate-gated node (`verify_predicate.is_some()`) also be recovery-eligible —
  `validate()` must reject this combination unconditionally (FR-011); recovery bypasses the
  completion-event handler where predicate verification runs.
- **NEVER** introduce a new `tokio::spawn()` call site for timeout enforcement or recovery — both
  are synchronous/inline per NFR-AS-01; per
  `[[039-background-task-supervisor/spec]]`'s binding NEVER section, this is a hard project-wide
  constraint, not specific to this feature.
- **NEVER** silently enforce `idle_timeout_secs` partially or heuristically in v1 (e.g. "best
  effort" using an unrelated existing signal) — it must be a clean no-op until the Alt A
  progress-signal plumbing exists, per FR-005's explicit no-partial-enforcement requirement.

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Per-task `run_timeout_secs` shorter than time already elapsed when set mid-run (e.g. hot-reloaded graph data) | Flagged as timed out on the next `check_timeouts()`/`wait_event()` evaluation — no special-cased grace period, consistent with existing `check_timeouts()` semantics |
| `idle_timeout_secs` set (per-task or global) | Documented no-op in v1 — never fires, never treated as "always idle" by omission; `--init`/config.toml text states this explicitly (FR-005). Additive completion (#6302, not a spec deviation): `DagScheduler::init_common()` also emits a one-time `tracing::warn!` per scheduler construction when the field is set anywhere in config or the graph, giving FR-005's "loudly marked reserved" requirement a runtime signal in addition to the static docs/wizard text — the warning only logs, it never enforces or partially enforces the timeout |
| `recovery.state_injection` set but effective strategy is `Skip` | `validate()` warns at graph-construction time; at runtime the task is `Skipped` via the existing `Skip` arm — recovery is never consulted (that arm does not call the recovery branch) |
| `recovery.state_injection` set but effective strategy is `Ask` | Same as `Skip` — `validate()` warns; runtime pauses the graph via the existing `Ask` arm, recovery is never consulted |
| `recovery.state_injection` set AND `verify_predicate` set on the same node | `validate()` rejects the graph at construction time (FR-011) — never reaches runtime |
| A node recovers, and its recovered failure also would have tripped a cascade-abort threshold, but the cascade check ran first and already aborted the graph | Graph ends `Failed` via `abort_dag_with_lineage()` — recovery is structurally unreachable, `propagate_failure()` (where recovery lives) was never called for this event (FR-013) |
| A node recovers via the timeout path (no cascade evaluation exists there) | Recovery always fires if `state_injection` is configured — there is no cascade check to preempt it on this path; this is the documented timeout-vs-event asymmetry (FR-014), not a bug |
| A node recovers; its failure was already recorded by `record_outcome(false)` before recovery ran (event path only) | The recorded failure still counts toward later cascade-threshold evaluation for other tasks in the same graph — documented v1 limitation (FR-015), not corrected in v1 |
| Both `run_timeout_secs` and (in a future Alt A build) `idle_timeout_secs` would fire on the same tick | Out of scope for v1 (idle is a no-op) — when Alt A ships, the timeout-cause record must name exactly one firing mechanism, not an aggregate flag (carried forward as a design note for the Alt A follow-up, NFR-OB-01) |
| Graph crash-resumes mid-tick, between a task's `Failed` status set and its Mode-1 recovery application | Cannot happen mid-persisted-state: the snapshot only ever captures the pre-tick or post-tick graph, never an intermediate state within a single synchronous `tick()` call (FR-016) — resume rebuilds `running` from the last persisted snapshot and the failure+recovery sequence re-fires cleanly on the next tick if it had not yet been captured |
| A recovered node's `agent_def == Some("__recovery__")` is inspected by code that assumes `agent_def` always names a real `SubAgentDef` | Out of scope to audit every `agent_def` consumer in this spec; flagged for the implementing session to grep for `agent_def` consumers and confirm none panics or mis-renders on this synthetic marker (implementation-phase verification, not a v1 requirement gap) |

---

## 6. Success Criteria

Implementation-facing checklist (business-facing criteria: `[[specs/075-orchestration-node-control-parity/brd]]` §6):

- [ ] Default (`timeout: None, recovery: None`) regression test: reproduces the exact pre-feature
      `check_timeouts()`/`wait_event()`/`propagate_failure()` code paths, byte-for-byte
- [ ] Per-task `run_timeout_secs` override test: fires before the (longer) global default would
      have, on both spawned and `RunInline` dispatch
- [ ] `idle_timeout_secs` no-op test: a task with a short `idle_timeout_secs` and long idle
      execution is never flagged as timed out by that field
- [ ] Mode-1 recovery test: a node with `state_injection` set, on `Abort`-default failure,
      transitions to `Completed`; its dependents unblock and receive the injected value through
      `build_task_prompt()`; `graph.status` stays `Running`
- [ ] Mode-1 recovery test: same, but for retry-exhausted `Retry` (not just `Abort`-default)
- [ ] Cascade-precedence test: a node with `recovery` configured whose failure also trips a
      cascade-abort threshold ends the graph `Failed`, not recovered
- [ ] `validate()` reject test: `recovery.is_some() && verify_predicate.is_some()` is rejected
      with a message naming the offending task
- [ ] `validate()` warn test: `recovery.is_some()` under effective `Skip`/`Ask` strategy warns but
      does not reject
- [ ] Metrics test: a recovered node is counted in `tasks_completed`, never `tasks_failed`, via
      the existing status-derived `finalize_plan_completed`/`finalize_plan_failed` path — no new
      metrics code required
- [ ] Config round-trip test: `default_idle_timeout_secs` serializes/deserializes correctly,
      defaults to `None`, and a config persisted before this feature exists migrates cleanly
- [ ] `cargo +nightly fmt --check`, `cargo clippy --profile ci ... -D warnings`,
      `cargo nextest run ...`, and the rustdoc gate all pass per `.claude/rules/branching.md`
- [ ] Zero new `tokio::spawn()` call sites: `.claude/rules/continuous-improvement.md`
      async-supervision scan count non-increasing
- [ ] `.local/testing/playbooks/orchestration-node-control-parity.md` created (main-repo path)
- [ ] `.local/testing/coverage-status.md` rows added (main-repo path, status `Untested`)
- [ ] `--init` wizard and `config.toml` comment for `default_idle_timeout_secs` verified live to
      state "reserved — not yet enforced"

---

## 7. Relationship to Existing Specs

| This spec | Existing spec | Relationship |
|-----------|---------------|---------------|
| `TaskNode.timeout`/`.recovery`, `propagate_failure()` recovery branch, `check_timeouts()`/`wait_event()` per-task awareness | `[[009-orchestration/spec]]` | Extends the existing `TaskGraph`/`DagScheduler` failure-handling and timeout model; does not change `FailureStrategy`'s enum shape, `Skip`/`Ask` semantics, or the cascade-detector's event-path ordering |
| No new `tokio::spawn` site, synchronous recovery mutation | `[[039-background-task-supervisor/spec]]` | Compliance claim verified against the binding NEVER section — see NFR-AS-01 |
| Original research/gap-audit spec | `.local/specs/059-orchestration-node-control-parity/spec.md` | This spec is the formal `/sdd` output resolving that draft's `[NEEDS CLARIFICATION]` items that are in-scope for v1; it also **narrows** that draft's scope — the research draft's `route_to`/Mode-2 sketch (its FR-004/FR-005, data-model `route_to` field) is retracted here as unsafe-as-designed (FR-D-01) rather than carried forward |
| Recovery-completed node skips predicate verification | `[[001-system-invariants/spec]]` | The `validate()` reject guard (FR-011) is the mechanism that keeps this consistent with any project-wide invariant about predicate-gated output never reaching downstream consumers unverified |

---

## 8. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[specs/075-orchestration-node-control-parity/brd]] — Business case and success criteria
- [[specs/075-orchestration-node-control-parity/srs]] — Full functional requirements (EARS)
- [[specs/075-orchestration-node-control-parity/nfr]] — Quality targets (ISO/IEC 25010)
- [[specs/075-orchestration-node-control-parity/plan]] — Step-by-step implementation plan
- [[specs/075-orchestration-node-control-parity/tasks]] — Ordered developer task breakdown
- [[001-system-invariants/spec]] — Cross-cutting architectural invariants
- [[009-orchestration/spec]] — DAG planner, `DagScheduler`, `TaskGraph`, parent spec for the
  orchestration subsystem this feature extends
- [[039-background-task-supervisor/spec]] — Binding async-supervision contract (NFR-AS-01)
- GitHub issue #6021 — source issue
- `.local/handoff/2026-07-13T21-12-06-architect.md` — final (v3) architect design
- `.local/handoff/2026-07-13T21-16-30-critic.md` — final critic verdict (minor / approved)
