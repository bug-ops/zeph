---
aliases:
  - Orchestration HITL Interrupt Spec
  - Spec 073
  - Declarative DAG Interrupt
tags:
  - sdd
  - spec
  - orchestration
  - security
created: 2026-07-13
status: draft
related:
  - "[[001-system-invariants/spec]]"
  - "[[009-orchestration/spec]]"
  - "[[064-durable-execution/spec]]"
  - "[[013-acp/spec]]"
  - "[[069-threat-model/spec]]"
  - "[[MOC-specs]]"
issues:
  - "#5918"
---

# Spec 073 — Declarative Task-Level HITL Interrupt for the Orchestration DAG

> [!info]
> Adds a resumable human-in-the-loop pause point to the `zeph-orchestration` task DAG: a task can
> be annotated to require operator input before it dispatches. The scheduler pauses the graph,
> the operator answers via `/plan provide`, and the answer is interpolated into the task's prompt
> before normal dispatch resumes. This is the honest, architecture-fitting parity story for
> LangGraph's `interrupt()`/`Command(resume=...)` pattern (issue #5918) — Zeph `TaskNode`s are
> declarative data, not executable closures, so a literal `interrupt()` call site does not exist;
> see §3.1 for why this reframing is correct rather than a reduced scope.

## Sources

### External
- LangGraph's `interrupt()` / `Command(resume=...)` human-in-the-loop pattern — the competitive
  parity target named in issue #5918 and tracked as a reference agent in
  `.claude/rules/continuous-improvement.md` ("Competitive Parity Monitoring").

### Internal

| File | Contents |
|---|---|
| `crates/zeph-orchestration/src/graph.rs` | `TaskNode` (`:378-452`, serde-default optional-field precedent: `network_scope`, `asset_sensitivity`, `execution_environment`, `token_budget_cents`); `GraphStatus` (`:233-259`, `#[non_exhaustive]`, `Paused` variant, `Display` impl); `TaskGraph` (`:502-529`); `GraphPersistence::save` (`:588-`, full-blob JSON write on every transition) |
| `crates/zeph-orchestration/src/dag.rs` | `ready_tasks` (`:179-`, filters purely on `TaskStatus::Ready`/`Pending` + predicate-clear parents — no gate awareness); `FailureStrategy::Ask` handler (`:299-301`, sets `graph.status = GraphStatus::Paused`); `reset_for_retry` |
| `crates/zeph-orchestration/src/scheduler/mod.rs` | `SchedulerAction` enum (`:63-`, `Spawn`/`RunInline`/`Done`/`VerifyPredicate`); `DagScheduler::new` (`:334-350`) and `resume_from` (`:389-405`) — both unconditionally set `graph.status = GraphStatus::Running` on construction, which is how a `Paused` graph resumes execution today |
| `crates/zeph-orchestration/src/scheduler/tick/mod.rs` | `tick()` (`:19-49`, short-circuits to `Done{status}` when `graph.status != Running` — no event draining on a paused tick); `dispatch_ready_tasks` (per-tick dispatch loop, marks `TaskStatus::Running` immediately before pushing `Spawn`/`RunInline`); `emit_pending_predicate_actions` (predicate gate, fires only for `Completed` tasks) |
| `crates/zeph-orchestration/src/scheduler/planner.rs` | `check_graph_completion` (`:105-`, declares deadlock+`Failed` when `running_in_graph_now == 0 && !all_terminal && dag::ready_tasks(&graph).is_empty()` — the false-deadlock hazard this spec must avoid, see §4) |
| `crates/zeph-orchestration/src/scheduler/router.rs` | `build_task_prompt` (`:18-`, builds the dispatched prompt from `task.description`) |
| `crates/zeph-orchestration/src/command.rs` | `PlanCommand` enum (`:16-31`: `Goal`/`Status`/`List`/`Cancel`/`Confirm`/`Resume`/`Retry`) + `parse()` |
| `crates/zeph-orchestration/src/planner.rs` | `PlannerResponse`/`PlannedTask` (`:139-163`, the narrow LLM-facing DTO — deliberately excludes `network_scope`/`asset_sensitivity`/`execution_environment`/`token_budget_cents`, establishing the precedent that security/policy-adjacent `TaskNode` fields are NOT LLM-authorable) |
| `crates/zeph-orchestration/src/plan_cache.rs` | `PlanTemplate`/`TemplateTask` (`:30-66`, skeleton extracted from a *completed* graph — not an authoring surface for new annotations) |
| `crates/zeph-core/src/agent/plan.rs` | `handle_plan_confirm` (`:385-`, calls `build_dag_scheduler` → `run_scheduler_loop`); `finalize_plan_execution` `Paused` arm (`:688-697`, prints "ask strategy" message, re-parks `pending_graph`); `handle_plan_resume_as_string` (`:1152-1209`, Path A: active-`pending_graph` status-gate; Path B: disk rehydration); `handle_plan_retry_as_string` (`:1211-1280`, gate at `:1234`, `reset_for_retry` + Running→Ready reset at `:1260-1264`) |
| `crates/zeph-config/src/experiment.rs` | `OrchestrationConfig` (`:261-`, existing `#[serde(default)]` bool-toggle precedent: `verify_completeness`, `topology_selection`) |
| `crates/zeph-config/src/migrate/steps.rs` | `MIGRATIONS` registry (85 sequential steps as of this spec) |
| `src/init/agents.rs` | `step_orchestration` — `--init` wizard section for `OrchestrationConfig` |
| `crates/zeph-acp/src/permission.rs` | `AcpPermissionGate` (`PermissionRequest.reply: oneshot::Sender`, `:51-55`; `reply_rx.await`, `:347`); `PersistedPermissions` (`:57-61`, TOML-persisted `AllowAlways` grants) — cited for the deferred ACP migration rationale (§8) |
| `crates/zeph-durable/src/promise.rs`, `crates/zeph-durable/src/ids.rs` | `DurablePromise`/`DurableHandle`/`PromiseId` (`ids.rs:323`, `Copy`+`Serialize`+`Deserialize`) — referenced only for the forward-compat schema hook (§3.3); zero new call sites in this spec's scope |

---

## 1. Overview

### Problem Statement

LangGraph's `interrupt()` lets a graph node pause mid-execution, surface a prompt to a human, and
resume with the human's answer returned at the call site. Issue #5918 asks for the equivalent in
Zeph's `zeph-orchestration` DAG. Today the only pause mechanism is `FailureStrategy::Ask`, which
is reactive (triggered by a task *failure*) and offers no way to *proactively* gate a task on
human approval or input before it ever dispatches. Separately, `AcpPermissionGate` blocks a single
tool call on IDE approval, but that is a different mechanism (ACP session-scoped, not DAG-scoped)
and does not compose with the orchestration graph.

### Goal

A plan author can mark a `TaskNode` as requiring human input before it dispatches. When the
scheduler reaches that task, it pauses the graph, exposes the prompt to the operator, and — once
the operator answers via `/plan provide <value>` — resumes dispatch with the answer interpolated
into the task's prompt. The pause is crash-resumable via the existing graph-blob persistence
mechanism, requires no new background tasks or parked awaits, and cannot be bypassed by `/plan
retry` or resolved by the LLM/sub-agent itself.

### Out of Scope

- **Phase 2 — imperative mid-execution interrupt.** A tool the *inner* agent can call mid-loop to
  request human input (the literal LangGraph `interrupt()` shape: pause *inside* a running task,
  not *before* it). Touches the agent tool loop, tool registry, and streaming — materially larger
  blast radius. File as a follow-up issue titled "orchestration: imperative mid-execution HITL
  interrupt tool (`request_human_input`) — Phase 2 of #5918", P3, gated on demonstrated need
  beyond what the declarative gate covers.
- **`AcpPermissionGate` → `DurablePromise` migration.** Assessed and deferred; see §8 for the
  rationale. File as a follow-up issue titled "acp: migrate `AcpPermissionGate` to `DurablePromise`
  for crash-resumable IDE approvals", P3, gated on a concrete need (background/A2A-relayed IDE
  approvals).
- **`DurablePromise`-backed resolution / A2A remote resolution.** Phase 1 ships the blob-only
  mechanism (ALT-1, §3.3). A `PromiseId` schema slot is reserved for a future upgrade but not
  wired up.
- **An authoring UI/API for setting `interrupt_before`.** `interrupt_before` is a public
  `TaskNode` field, programmatically settable by any code constructing a `TaskGraph` (future plan
  template tooling, custom orchestration call sites, tests). It is deliberately excluded from the
  LLM-facing `PlannedTask`/`PlannerResponse` schema (§3.1) — no live planner LLM call can set it
  in v1. Building a template-editing UI or a `/plan annotate` command is separate future work.
- **Validating `resolved_input` against `InterruptRequest.schema`.** The `schema` field is an
  advisory hint for a future structured-input UI; v1 does not validate the operator's answer
  against it.
- **A TTL/expiry mechanism for unanswered interrupts.** Matches the existing indefinite lifetime
  of an `Ask`-strategy pause (see §4 C3).

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `orchestration.interrupt_enabled = true` AND the scheduler's dispatch loop reaches a `Ready` task with `interrupt_before.is_some()` AND `resolved_input.is_none()` THE SYSTEM SHALL NOT dispatch that task, SHALL set `graph.status = Paused` and `graph.pause_reason = Some(PauseReason::AwaitingInput{..})`, and SHALL stop emitting further dispatch actions for the remainder of that tick | must |
| FR-002 | WHEN `orchestration.interrupt_enabled = false` (default) OR a task's `interrupt_before` is `None` THE SYSTEM SHALL dispatch that task exactly as today, with no gate evaluation | must |
| FR-003 | WHEN the tick loop has already pushed `Spawn`/`RunInline` actions for one or more Ready tasks earlier in the same tick, and a later task in that same tick's ready-list is interrupt-gated, THE SYSTEM SHALL keep the earlier actions (do not retract already-emitted dispatches) and only suppress dispatch for the gated task and any tasks after it in the list | must |
| FR-004 | WHEN the graph pauses for an interrupt gate THE SYSTEM SHALL leave the gated `TaskNode.status` as `Ready` (never transition it to any other status while paused) | must |
| FR-005 | WHEN the operator runs `/plan provide <value>` against the active `pending_graph` AND `graph.status == Paused` AND `graph.pause_reason` is `Some(PauseReason::AwaitingInput{task_id, ..})` THE SYSTEM SHALL parse `<value>` as JSON if it parses, else as a plain string, set `graph.tasks[task_id].resolved_input = Some(value)`, clear `graph.pause_reason = None`, persist the updated graph blob immediately (if `graph_persistence` is enabled), and instruct the operator to run `/plan confirm` | must |
| FR-006 | WHEN `/plan provide` is invoked but `pending_graph` is `None`, or `graph.status != Paused`, or `graph.pause_reason` is not `Some(AwaitingInput{..})` THE SYSTEM SHALL reject with a message naming the actual state and the correct next command | must |
| FR-007 | WHEN `/plan confirm` re-enters a `Paused` graph (via `DagScheduler::new`) THE SYSTEM SHALL set `graph.status = Running` (existing behavior, unchanged) so the scheduler proceeds; a task whose `resolved_input.is_some()` now dispatches normally | must |
| FR-008 | WHEN a task with `interrupt_before.is_some()` has `resolved_input.is_some()` THE SYSTEM SHALL interpolate the resolved value into the prompt built by `build_task_prompt` before dispatch, and SHALL NOT re-evaluate the gate (dispatch proceeds unconditionally once resolved) | must |
| FR-009 | WHEN `handle_plan_retry_as_string` is invoked on a graph whose `pause_reason` is `Some(PauseReason::AwaitingInput{..})` THE SYSTEM SHALL reject the retry with a message directing the operator to `/plan provide` first, and SHALL NOT call `reset_for_retry` | must |
| FR-010 | WHEN `/plan cancel` is invoked on a graph paused for an interrupt gate THE SYSTEM SHALL behave exactly as it does for any other `Paused` graph today (set `Canceled`, no special-case cleanup — there is no promise row to garbage-collect under ALT-1) | must |
| FR-011 | WHEN a process crashes after `graph.pause_reason = Some(AwaitingInput{..})` has been persisted but before the operator answers THE SYSTEM SHALL allow `/plan resume <id>` to rehydrate the graph from disk with the pause state intact, exactly as an `Ask`-strategy pause already does | must |
| FR-012 | WHEN `zeph-config --migrate-config` runs on a pre-073 config THE SYSTEM SHALL add `interrupt_enabled = false` to the `[orchestration]` section of every migrated config | must |
| FR-013 | WHEN `--init` runs the orchestration wizard step (`step_orchestration`) THE SYSTEM SHALL prompt for interrupt-gate enablement, defaulting to `No` | should |
| FR-014 | THE SYSTEM SHALL NOT expose `interrupt_before` in `PlannedTask`/`PlannerResponse` (the LLM-facing planner schema) — mirrors the existing exclusion of `network_scope`/`asset_sensitivity` | must |

---

## 3. Architecture

### 3.1 Why a declarative pre-dispatch gate, not literal `interrupt()`

A `TaskNode` (`graph.rs:378`) carries no executable body — `title`/`description`/`agent_hint`
etc. are pure data. The scheduler's dispatch loop builds a prompt (`build_task_prompt`) and emits
`SchedulerAction::Spawn`/`RunInline`; `zeph-core` then dispatches that prompt to a sub-agent or
runs it inline. There is no in-process point where "task logic calls `interrupt()` and gets the
value back at the call site" could exist — the task has no logic, only a prompt. The maximal
faithful parity is therefore a **pre-dispatch gate**: annotate a task as needing human input
before dispatch, pause the graph when the scheduler reaches it, and inject the operator's answer
into the prompt that eventually *does* dispatch. This is a **known parity delta** from LangGraph:
the sub-agent sees the human input as prompt text, not as a typed value returned at a call site
(see FR-008; a structured-injection alternative is Phase-2 territory, not v1).

### 3.2 Data Model

```rust
// graph.rs, alongside NetworkScope / AssetSensitivity

/// A request for human input before a task dispatches.
///
/// Set by whatever constructs the `TaskGraph` (plan template tooling, a custom
/// orchestration call site, tests) — deliberately NOT exposed to the live planner
/// LLM (see `PlannedTask`, planner.rs:146, and FR-014).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptRequest {
    /// Operator-facing question, shown verbatim when the graph pauses.
    pub prompt: String,
    /// Advisory schema hint for a future structured-input UI. Not validated in v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

// TaskNode additions (same `#[serde(default, skip_serializing_if = "Option::is_none")]`
// pattern as `network_scope`/`asset_sensitivity`/`execution_environment`/`token_budget_cents`):

/// Declares this task needs human input before it dispatches. `None` = no gate
/// (today's behavior). See spec 073.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub interrupt_before: Option<InterruptRequest>,

/// Operator-supplied answer, set by the `/plan provide` command handler.
/// Consumed by `build_task_prompt` to interpolate into the dispatched prompt.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub resolved_input: Option<serde_json::Value>,
```

```rust
// graph.rs, alongside GraphStatus

/// Disambiguates *why* a `TaskGraph` is `Paused`. `TaskGraph.pause_reason == None`
/// means either "not paused" or the legacy `FailureStrategy::Ask` pause (today's only
/// producer of `GraphStatus::Paused`) — both are indistinguishable today and require
/// no new handling. `Some(AwaitingInput{..})` is the new declarative interrupt gate.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PauseReason {
    AwaitingInput {
        task_id: TaskId,
        prompt: String,
        /// Forward-compat hook for a future `DurablePromise` upgrade (Phase 2 territory,
        /// where INV-9's resolver-token threat model actually applies — see §3.3, §7).
        /// Always `None` in Phase 1; reserved so that upgrade needs no blob re-serialization.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        promise_id: Option<zeph_durable::PromiseId>,
    },
}
```

```rust
// TaskGraph addition
#[serde(default, skip_serializing_if = "Option::is_none")]
pub pause_reason: Option<PauseReason>,
```

`GraphStatus::Paused` is **reused**, not extended with a new variant, on two grounds (per critic
review, not primarily "less match-site churn" — both designs require the same semantic audit at
`plan.rs:688`/`:1234`):
(a) `GraphStatus` serialization stability — it is stored as a plain string column
(`graph.status.to_string()`, `graph.rs:601`) and adding a variant is schema-additive risk-free,
but a *second* terminal-adjacent status would fragment every status-string consumer;
(b) keeping all `Paused`-handling code paths (resume/retry/status/list/finalize) unified rather
than fragmented across two status values. The one exhaustive in-crate match — `Display` at
`graph.rs:248-259` — needs no change either way since `Paused` itself is untouched; only
`pause_reason` is new.

### 3.3 ALT-1 (blob-only) chosen over `DurablePromise` for Phase 1

**Decision:** `resolved_input` + `pause_reason` live on the already-durable `TaskGraph`/`TaskNode`
blob (`GraphPersistence::save`, `graph.rs:588`, writes the full JSON blob on every transition —
the same mechanism `FailureStrategy::Ask` pauses already rely on for crash-resumability). **No
`DurablePromise` is introduced in Phase 1.**

Reconciliation with spec 064 (durable execution) and spec 001 (system invariants):

- **INV-9 (spec 064:116-121, "`PromiseId` is NOT a bearer capability... HITL resolution is
  operator-channel-only; the LLM MUST NOT be able to resolve its own pending promises") does not
  bite this gate.** INV-9 defends against a party that can *reach* the resolve path forging a
  resolution. In Phase 1 the only resolve path is the `/plan provide` **command handler running in
  `zeph-core`**, structurally unreachable from a sub-agent — a sub-agent receives a prompt and
  returns a result string; it has no write access to the graph blob or the command registry. There
  is no "compromised inner agent forges its own resolution" surface for a *pre-dispatch* gate (this
  becomes real in Phase 2, where the LLM is *inside* the tool loop and could in principle call a
  `resolve`-shaped tool — which is exactly why Phase 2 is the natural home for `DurablePromise`).
- **INV-1 (spec 064:67-70, "Layer 0 has no business-logic dependencies") is not implicated either
  way** — `zeph-orchestration` already depends on `zeph-durable` (workspace dependency, used by the
  existing P2 `journal_budget`/`restore_budget` adapter in `zeph-orchestration/src/durable.rs`);
  spec 064:314-315 explicitly anticipates `zeph-orchestration` as an L3 consumer. Choosing ALT-1 is
  a scope/timing decision, not a legality one.
- **Durability is already covered without a promise**: `GraphPersistence::save` persists the full
  blob on every transition; `Ask` pauses are already blob-only and crash-resumable in production.
  `resolved_input`/`pause_reason` inherit that for free — no new persistence code path.
- **A2A/remote resolution is explicitly out of v1** (operator-channel-only, `LocalBackend`-only,
  matching the existing P2 adapter's scope, which hard-errors on non-Local backends,
  `orchestration/src/durable.rs:209`).
- **`DurablePromise` would add real cost with no v1 payoff**: net-new promise mint/store/resolve
  wiring in a crate that today uses only zeph-durable's *journal* API (not its *promise* API), a
  resolver-token-delivery scheme to the operator channel, and a cancel-time GC obligation for the
  promise row that ALT-1 simply does not have (see FR-010).

**Forward-compat hook:** `PauseReason::AwaitingInput.promise_id: Option<PromiseId>` is reserved
now (`#[serde(default)]`, always `None` in Phase 1) so a later `DurablePromise` upgrade — bundled
with Phase 2, where INV-9 genuinely applies — needs no graph-blob re-serialization.

### 3.4 Data Flow

1. `dispatch_ready_tasks` (`scheduler/tick/mod.rs`) iterates the tick's ready-task list in order.
   For each task, if `orchestration.interrupt_enabled && task.interrupt_before.is_some() &&
   task.resolved_input.is_none()`: set `self.graph.status = Paused`,
   `self.graph.pause_reason = Some(AwaitingInput{task_id, prompt: interrupt_before.prompt.clone(),
   promise_id: None})`, and `break` out of the dispatch loop — do not touch this task's
   `TaskStatus` (it stays `Ready`, see §4 invariant on the false-deadlock hazard). Tasks earlier in
   the same ready-list iteration that were already dispatched (`Spawn`/`RunInline` pushed) keep
   their actions (FR-003).
2. `tick()` continues past `dispatch_ready_tasks` (no early-return gate exists there today) into
   `emit_pending_predicate_actions()` (harmless — operates only on `Completed` tasks, unaffected
   by the pause) and `check_graph_completion()`. Because the gated task's `TaskStatus` was left
   `Ready`, `dag::ready_tasks(&graph)` still returns it — `check_graph_completion`'s deadlock branch
   (`running_in_graph_now == 0 && !all_terminal && ready_tasks(&graph).is_empty()`) is **not**
   triggered even when the gated task is the sole non-terminal task with zero concurrently-`Running`
   siblings in that tick. (If this invariant were violated — e.g., by transitioning the gated task
   to some other status while paused — the graph would spuriously flip to `Failed` and cancel every
   other task in the same tick this gate fires. This is the single most important implementation
   constraint in this spec; see §4 and the dedicated test in `tasks.md`.)
3. `run_scheduler_loop` returns with `Done{status: Paused}`; `finalize_plan_execution`
   (`plan.rs:688`) branches on `pause_reason`: for `Some(AwaitingInput{prompt, ..})` it prints the
   operator-facing prompt and how to answer (`/plan provide <value>`), then re-parks
   `pending_graph`. For `None` (legacy) it prints today's "ask strategy" message, unchanged.
   Already-`Running` siblings from this or prior ticks are unaffected by this whole path — they
   drain through the *existing* event-processing mechanism identically to how `Ask`-pause siblings
   already drain today; this spec introduces no new sibling-draining logic (see §4).
4. Operator runs `/plan provide <value>` → `handle_plan_provide_as_string` (new handler, mirrors
   `handle_plan_retry_as_string`'s active-`pending_graph`-only gating, `plan.rs:1211`): validates
   `pending_graph.is_some() && status == Paused && pause_reason == Some(AwaitingInput{task_id,..})`;
   parses `<value>` as JSON, falling back to a plain string on parse failure; sets
   `graph.tasks[task_id].resolved_input = Some(value)`; clears `pause_reason = None`; persists the
   graph immediately via `graph_persistence.save(&graph)` if enabled (crash-safety — without this,
   a crash between `/plan provide` and `/plan confirm` loses the answer, since nothing else saves
   the blob until the next scheduler-driven transition); tells the operator to run `/plan confirm`.
5. Operator runs `/plan confirm` → `handle_plan_confirm` (`plan.rs:385`) → `build_dag_scheduler` →
   `DagScheduler::new` (`scheduler/mod.rs:334`) unconditionally sets `graph.status = Running`
   (existing behavior — this is how an `Ask` pause already resumes, no new code needed) →
   `run_scheduler_loop`. The next `tick()` reaches the same task again: `interrupt_before.is_some()
   && resolved_input.is_some()` → the gate condition in step 1 is false, so `dispatch_ready_tasks`
   dispatches normally. `build_task_prompt` interpolates `resolved_input` into the prompt (FR-008)
   before `Spawn`/`RunInline` is emitted.
6. Crash at any point between (2) and (5): the graph blob already has `pause_reason` (and, once
   step 4 ran, `resolved_input`) persisted. `/plan resume <id>` (`plan.rs:1152`) rehydrates from
   disk exactly as it does for an `Ask` pause today, and the operator continues from wherever they
   left off (`/plan provide` if not yet answered, `/plan confirm` if already answered).

### 3.5 Prompt Interpolation (FR-008)

`build_task_prompt` (`scheduler/router.rs:18`), when `task.resolved_input.is_some()`, appends a
clearly delimited section to the built prompt:

```
{existing prompt from task.description}

--- Human-provided input ---
{resolved_input rendered: if Value::String(s), s verbatim; otherwise pretty-printed JSON}
---
```

This is the stated parity delta from §3.1: the sub-agent receives the human's answer as prompt
text, not as a typed return value at a call site.

### 3.6 Predicate Gate Ordering (no collision)

`interrupt_before` is evaluated at **Ready→dispatch** (`dispatch_ready_tasks`); `verify_predicate`
is evaluated at **Completed→downstream-unblock** (`emit_pending_predicate_actions`, fires only for
`TaskStatus::Completed` tasks). These are disjoint lifecycle phases on the same node: a task passes
its interrupt gate (if any) first, dispatches, completes, and only then is its predicate (if any)
evaluated. They cannot collide or interleave.

### 3.7 Non-Blocking Contract Compliance (MANDATORY per CLAUDE.md)

The pause-and-return / resolve-on-next-command flow spans two separate command invocations
(`/plan provide`, `/plan confirm`) — no await is parked across a turn, exactly like the existing
`Ask` pause. Zero new `tokio::spawn` call sites are introduced; the scheduler's existing tick-based
event loop is reused unchanged. Acceptance test: re-run the project's `tokio::spawn` scan command
(`.claude/rules/continuous-improvement.md`, "Async Supervision Audit") before and after this PR —
the count must not increase.

### 3.8 Config

```toml
[orchestration]
interrupt_enabled = false   # opt-in: honor TaskNode.interrupt_before gates (spec 073, #5918)
```

`OrchestrationConfig.interrupt_enabled: bool` (`zeph-config/src/experiment.rs`), `#[serde(default)]`
→ `false`, mirroring the existing `verify_completeness` opt-in-bool precedent in the same struct.
When `false`, `interrupt_before` on any task is inert (FR-002) — safe default for headless/gateway/
scheduler-triggered plans where no operator is present to answer.

---

## 4. Key Invariants

### Always (without asking)
- Old stored graph blobs (pre-073) deserialize unchanged: `interrupt_before`, `resolved_input`,
  `pause_reason` all default to `None` via `#[serde(default, skip_serializing_if =
  "Option::is_none")]`, identical to the `network_scope`/`asset_sensitivity` precedent.
- `interrupt_before` is evaluated only at Ready→dispatch; `verify_predicate` only at
  Completed→downstream-unblock (§3.6) — never reordered, never evaluated twice for the same
  transition.
- An interrupt-gated `TaskNode`'s `TaskStatus` remains `Ready` for the entire time the graph is
  `Paused` for that reason. This is what keeps `dag::ready_tasks()` non-empty and prevents
  `check_graph_completion`'s deadlock branch from firing spuriously (§3.4 step 2).
- Already-`Running` siblings (from this tick or prior ticks) are unaffected by the pause and drain
  through the pre-existing event-processing path — no new draining logic is introduced.
- `/plan provide` persists the graph blob immediately after mutating `resolved_input` (if
  persistence is enabled) — a crash before `/plan confirm` must not silently discard the answer.

### Ask First
- Adding a distinct `GraphStatus` variant instead of the `pause_reason` field (rejected for
  Phase 1 on the grounds in §3.2; revisit only if a future design genuinely needs
  `Paused`-handling code paths to diverge, which none currently do).
- Wiring `DurablePromise`/A2A resolution into this gate (the reserved `promise_id` schema slot is
  a hook, not a commitment — actually using it is a Phase-2-scale architectural decision).

### Never
- `/plan retry` MUST NOT call `reset_for_retry` on a graph whose `pause_reason` is
  `Some(AwaitingInput{..})` (FR-009) — that would silently re-dispatch the gated task without the
  operator's value, bypassing the human gate entirely.
- The LLM/sub-agent MUST NOT be able to set `resolved_input` or clear `pause_reason` — the only
  mutation path is the `/plan provide` command handler in `zeph-core`, structurally unreachable
  from a dispatched sub-agent's return value (§3.3).
- `interrupt_before` MUST NOT appear in `PlannedTask`/`PlannerResponse` (FR-014) — no live planner
  LLM call may author an interrupt gate.
- No new `tokio::spawn` call site; no await held across the pause (§3.7).
- No TTL/expiry for an unanswered interrupt pause (§ Edge Cases, C3) — matches `Ask` pause
  lifetime exactly; do not add one without a separate spec.

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Same tick: earlier Ready tasks dispatched, then an interrupt-gated task is reached | Earlier dispatch actions (`Spawn`/`RunInline`) are kept; the gate stops only further dispatch in that tick (FR-003) |
| Already-`Running` siblings (this tick or prior) when the pause triggers | Drain naturally via the existing event-processing mechanism, identical to today's `Ask`-pause behavior — no new logic |
| Interrupt-gated task is the sole non-terminal task, zero `Running` siblings, in the tick that triggers the pause | `TaskStatus` stays `Ready` (not mutated) so `dag::ready_tasks()` is non-empty and `check_graph_completion`'s deadlock branch does not spuriously fire `Failed` + cancel-all (§3.4 step 2 — see dedicated regression test in `tasks.md`) |
| `/plan retry` on a graph with `pause_reason = Some(AwaitingInput{..})` | Rejected; message directs the operator to `/plan provide` first (FR-009) |
| `/plan cancel` on an interrupt-gated pause | Identical to any other `Paused` graph — `Canceled`, nothing to garbage-collect under ALT-1 (FR-010) |
| Operator never answers | Pause persists indefinitely until `/plan cancel`, identical lifetime to an unanswered `Ask` pause (C3) — no new TTL mechanism |
| Crash between the pause and `/plan provide` | Graph blob already has `pause_reason` persisted (written by the transition that set `Paused`); `/plan resume <id>` rehydrates; operator answers as normal (FR-011) |
| Crash between `/plan provide` and `/plan confirm` | `/plan provide` persists immediately (Always-invariant above), so the resolved value survives; `/plan resume <id>` rehydrates with `resolved_input` already set, `pause_reason` already cleared; operator runs `/plan confirm` |
| `orchestration.interrupt_enabled = false` but a task has `interrupt_before` set (e.g., stale config toggle, or a graph authored under a different config) | Gate is inert — dispatches immediately, exactly as if `interrupt_before` were `None` (FR-002) |
| Operator supplies a value that fails to parse as JSON (e.g., free-text prose) | Falls back to a plain `serde_json::Value::String` — no error, no rejection (FR-005) |
| `/plan provide` invoked with no active `pending_graph`, or active graph not `Paused` for `AwaitingInput` | Rejected with a message naming the actual state and the correct next command (FR-006) |

---

## 6. Success Criteria

- [ ] A task with `interrupt_before` set pauses the graph at Ready→dispatch when
      `interrupt_enabled = true`; does not pause when `false` (FR-001, FR-002)
- [ ] `/plan provide <value>` resolves the gate, persists immediately, and `/plan confirm` dispatches
      the task with the value interpolated into its prompt (FR-005, FR-007, FR-008)
- [ ] `/plan retry` is rejected on an `AwaitingInput` pause (FR-009); `/plan cancel` behaves
      identically to any other `Paused` graph (FR-010)
- [ ] Old graph blobs (no `interrupt_before`/`resolved_input`/`pause_reason` fields) deserialize
      without error and behave exactly as before this spec
- [ ] Regression test proves `check_graph_completion` does NOT flip to `Failed`+cancel-all when an
      interrupt-gated task is the sole ready task with zero running siblings (§3.4 step 2)
- [ ] `tokio::spawn` scan count (`.claude/rules/continuous-improvement.md`) does not increase after
      this PR
- [ ] `cargo nextest run --workspace --features "desktop,ide,server,chat,pdf,scheduler,testing" --lib --bins`
      passes with new tests added, no regressions
- [ ] `--migrate-config` adds `interrupt_enabled = false` to pre-073 configs (FR-012)
- [ ] `.local/testing/playbooks/orchestration-hitl-interrupt.md` and
      `.local/testing/coverage-status.md` updated per plan.md task list

---

## 7. Open Questions

None — all open questions raised by the architect (OQ-1 through OQ-5) were resolved by the critic
review and encoded above: OQ-1 → ALT-1 blob-only (§3.3); OQ-2 → prompt interpolation, stated as a
known parity delta (§3.1, §3.5); OQ-3 → `/plan provide <value>` as a dedicated subcommand, always
operating on the active `pending_graph` only, no separate `graph-id` argument (§3.4 step 4 — a
deliberate parser simplification: unlike `resume`/`retry`, "provide" only makes sense against the
plan that is actively paused in the current session; an operator wanting to answer a different
persisted graph's interrupt must `/plan resume <id>` first); OQ-4 → confirmed, this spec is Phase 1
only; OQ-5 → no hazard, disjoint lifecycle phases (§3.6).

---

## 8. ACP Permission Gate Migration — Deferred (context for the follow-up issue)

`AcpPermissionGate` (`permission.rs`) awaits IDE approval over an in-memory `oneshot`
(`reply_rx.await`, `:347`). A crash during that await means the tool call never executed (the
await errors, the turn fails); already-granted `AllowAlways` decisions are TOML-persisted
(`PersistedPermissions`, `:57-61`), so the IDE re-drives the session on reconnect with no
lost-approval-proceeds gap. There is no "lost approval causes an unapproved action to proceed"
hazard. Migrating to `DurablePromise` would let a pending approval survive a process crash, but the
benefit rarely materializes for a short-lived, stateful, single-stdio-connection IDE session, and
the cost is real (pulls `durable_promises` + backend/cipher wiring into a currently durable-free
crate, plus a new resolver-token-delivery scheme to the IDE). Deferred; see Out of Scope for the
follow-up issue to file.

---

## 9. See Also

- [[MOC-specs]] — Map of all specifications
- [[001-system-invariants/spec]] — INV-9-adjacent reasoning in §3.3
- [[009-orchestration/spec]] — DAG planner, scheduler, `/plan` command family this spec extends
- [[064-durable-execution/spec]] — `DurablePromise`, INV-1, INV-9; why ALT-1 was chosen over it for
  Phase 1
- [[013-acp/spec]] — `AcpPermissionGate`, the deferred migration target
- [[069-threat-model/spec]] — precedent for advisory-only `TaskNode` annotations excluded from the
  live planner LLM schema
