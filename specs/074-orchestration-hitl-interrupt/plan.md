---
aliases:
  - Orchestration HITL Interrupt Plan
  - Plan 073
tags:
  - sdd
  - plan
  - orchestration
created: 2026-07-13
status: draft
related:
  - "[[074-orchestration-hitl-interrupt/spec]]"
  - "[[074-orchestration-hitl-interrupt/tasks]]"
---

# Implementation Plan 073 — Declarative Task-Level HITL Interrupt

## Overview

One PR, non-trivial but self-contained (per architect's scope estimate). No new crate. Five
touched crates: `zeph-orchestration` (data model + scheduler gate), `zeph-core` (`/plan provide`
handler + prompt injection call site), `zeph-config` (`interrupt_enabled` toggle + migration
step), `zeph-commands` (`PlanCommand::Provide` dispatch), `src/init` (wizard prompt). No changes
to `zeph-durable` (the reserved `promise_id` field is read from `zeph-durable::PromiseId` but
nothing in this PR mints, stores, or resolves a promise) or `zeph-acp` (migration deferred, §8 of
spec.md).

Full CI gate (fmt/clippy/nextest/rustdoc, `.claude/rules/branching.md`) applies to this single PR.

**LLM-serialization gate:** NOT triggered — this feature does not touch
`crates/zeph-llm/src/claude.rs`/`openai.rs`/`ollama.rs`/`compatible.rs`, `MessagePart`, or context
assembly. `build_task_prompt`'s output is plain prompt text consumed the same way it always was;
no new serialization surface to the LLM API is introduced.

---

## 1. Data Model (`zeph-orchestration/src/graph.rs`)

Add, in the same block as `NetworkScope`/`AssetSensitivity` (near `graph.rs:340-452`):

```rust
/// A request for human input before a task dispatches. See spec 073.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptRequest {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}
```

`TaskNode` additions (two new optional fields, same serde pattern as the four precedent fields):

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub interrupt_before: Option<InterruptRequest>,

#[serde(default, skip_serializing_if = "Option::is_none")]
pub resolved_input: Option<serde_json::Value>,
```

`PauseReason` (new enum, non_exhaustive, placed near `GraphStatus`):

```rust
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PauseReason {
    AwaitingInput {
        task_id: TaskId,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        promise_id: Option<zeph_durable::PromiseId>,
    },
}
```

`TaskGraph` addition:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub pause_reason: Option<PauseReason>,
```

`TaskNode::new` constructor: no change needed (new fields default via `#[serde(default)]` and,
for a Rust-constructed struct literal, via `..Default::default()`-equivalent explicit `None` —
check whether `TaskNode::new` uses a full struct literal; if so, add `interrupt_before: None,
resolved_input: None` to it explicitly, matching how `network_scope`/`asset_sensitivity` were
added).

Doc-test update: extend the existing `TaskNode` module doc example (`graph.rs:~370`, currently
asserts `network_scope.is_none()` / `asset_sensitivity.is_none()`) with
`assert!(node.interrupt_before.is_none())`.

---

## 2. Scheduler Gate (`zeph-orchestration/src/scheduler/`)

### 2.1 `tick/mod.rs::dispatch_ready_tasks`

Inside the `for task_id in ready { ... }` loop, immediately after `let task =
&self.graph.tasks[task_id.index()];` and before the `execution_mode == Sequential` check, insert:

```rust
if self.interrupt_enabled
    && let Some(ref req) = task.interrupt_before
    && task.resolved_input.is_none()
{
    self.graph.status = GraphStatus::Paused;
    self.graph.pause_reason = Some(PauseReason::AwaitingInput {
        task_id,
        prompt: req.prompt.clone(),
        promise_id: None,
    });
    self.graph_dirty = true;
    break;
}
```

`self.interrupt_enabled: bool` is a new `DagScheduler` field, threaded from
`OrchestrationConfig.interrupt_enabled` at scheduler construction time (`DagScheduler::new` /
`build_dag_scheduler` in `plan.rs:164`). **Do not** mutate `task.status` — it must remain `Ready`
(spec.md §3.4 step 2, §4 invariant). `break` (not `continue`) so no task later in the same
ready-list is dispatched or gate-checked this tick, satisfying FR-003's "keep earlier actions,
suppress the rest."

### 2.2 `scheduler/router.rs::build_task_prompt`

After the existing prompt-construction logic, before returning:

```rust
if let Some(ref value) = task.resolved_input {
    let rendered = match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    prompt.push_str(&format!("\n\n--- Human-provided input ---\n{rendered}\n---"));
}
```

### 2.3 `scheduler/planner.rs::check_graph_completion` — verification only, no code change expected

Trace through the invariant from spec.md §3.4 step 2: because §2.1 never mutates `task.status`,
`dag::ready_tasks(&self.graph)` continues to report the gated task, so the `ready_tasks(&graph)
.is_empty()` deadlock check at `planner.rs:136` stays `false` and the `Failed`+cancel-all branch is
never spuriously entered. **This is verified by a dedicated regression test (T-009 in tasks.md),
not a code change** — the existing deadlock-detection logic is correct as-is *provided* §2.1 keeps
its "never touch `task.status`" contract. If a future refactor of `dispatch_ready_tasks` ever
changes that contract, this test will catch the regression.

### 2.4 `DagScheduler` construction

`build_dag_scheduler` (`plan.rs:164`) reads `self.config_for_orchestration().interrupt_enabled`
and passes it into `DagScheduler::new`/`resume_from` alongside the existing config-derived
scheduler parameters (`max_parallel`, `default_failure_strategy`, etc. — follow the existing
parameter-threading pattern in that constructor).

---

## 3. `/plan provide` Command (`zeph-orchestration/src/command.rs`, `zeph-core/src/agent/plan.rs`,
`zeph-commands/src/handlers/plan.rs`)

### 3.1 `PlanCommand` enum

```rust
/// `/plan provide <value>` — answer a pending interrupt-gate pause. Operates only on
/// the active `pending_graph` (see spec 073 §7 OQ-3 resolution: unlike resume/retry,
/// this has no separate graph-id argument).
Provide(String),
```

`parse()` (`command.rs:39-`): after the existing `strip_prefix("/plan")` + subcommand-token
dispatch, add a `"provide"` branch that takes the **entire remainder** (untrimmed of internal
whitespace, only outer-trimmed) as the value string — do not tokenize further, since the value may
itself contain spaces or be raw JSON.

### 3.2 `handle_plan_provide_as_string` (new, `zeph-core/src/agent/plan.rs`, placed next to
`handle_plan_retry_as_string` at `:1211`)

```rust
pub(super) async fn handle_plan_provide_as_string(
    &mut self,
    raw_value: &str,
) -> Result<String, error::AgentError> {
    use zeph_orchestration::{GraphStatus, PauseReason};

    let Some(ref graph) = self.services.orchestration.pending_graph else {
        return Ok("No active plan awaiting input. Use `/plan resume <id>` first \
                    if you have a persisted paused plan.".to_owned());
    };

    let Some(PauseReason::AwaitingInput { task_id, .. }) = graph.pause_reason else {
        return Ok(format!(
            "The active plan is in '{}' status and is not awaiting input. \
             Use `/plan status` to inspect it.",
            graph.status
        ));
    };
    debug_assert_eq!(graph.status, GraphStatus::Paused);

    let value: serde_json::Value = serde_json::from_str(raw_value)
        .unwrap_or_else(|_| serde_json::Value::String(raw_value.to_owned()));

    let mut graph = self.services.orchestration.pending_graph.take()
        .expect("just checked Some");
    graph.tasks[task_id.index()].resolved_input = Some(value);
    graph.pause_reason = None;

    if let Some(ref persistence) = self.services.orchestration.graph_persistence {
        if let Err(e) = persistence.save(&graph).await {
            tracing::warn!(graph_id = %graph.id, error = %e,
                "failed to persist resolved interrupt input; value is memory-only \
                 until /plan confirm runs");
        }
    }

    let msg = "Input recorded. Use `/plan confirm` to continue execution.".to_owned();
    self.services.orchestration.pending_graph = Some(graph);
    Ok(msg)
}
```

Note the `let Some(...) = graph.pause_reason else` pattern borrows `graph` immutably first (to
read `pause_reason` and produce error messages without an early `.take()`), matching the existing
style in `handle_plan_retry_as_string` (borrow-check-then-take). Adjust for `PauseReason` not being
`Copy` if needed (it isn't — `String` field) by matching on `&graph.pause_reason` and cloning
`task_id` (which is `Copy`) out before the `take()`.

### 3.3 Dispatch wiring

`handle_plan_command_as_string` (`plan.rs:1282`, `match cmd { PlanCommand::Retry(id) => ...,
PlanCommand::Provide(value) => self.handle_plan_provide_as_string(&value).await?, ... }`) and the
`zeph-commands/src/handlers/plan.rs` registry entry (mirror the existing `resume`/`retry` entries —
usage string, autocomplete hint).

---

## 4. Config (`zeph-config/src/experiment.rs`)

```rust
/// Enable the declarative pre-dispatch HITL interrupt gate (`TaskNode.interrupt_before`).
/// When `false` (default), `interrupt_before` on any task is inert — dispatches immediately.
/// See spec 073 / GitHub #5918.
#[serde(default)]
pub interrupt_enabled: bool,
```

Added to `OrchestrationConfig` next to `verify_completeness` (same `#[serde(default)]` bool
pattern). Update the struct's rustdoc module example if one enumerates all fields.

---

## 5. `--migrate-config` (`zeph-config/src/migrate/steps.rs`)

Add migration step 86 (current registry has 85, per `migrate/tests.rs:1784`):

```rust
// migrate/steps.rs — new struct following the existing one-struct-per-step pattern
pub(super) struct AddOrchestrationInterruptEnabled;

impl Migration for AddOrchestrationInterruptEnabled {
    fn name(&self) -> &'static str { "add-orchestration-interrupt-enabled" }
    fn apply(&self, toml_src: &str) -> String {
        // insert `interrupt_enabled = false` into the `[orchestration]` section,
        // following the same insert_after_section / merge_table_commented helpers
        // used by the other OrchestrationConfig-touching migration steps
    }
}
```

Register in the `MIGRATIONS` static (`migrate/mod.rs:646`). Update `migrate/tests.rs:1784`'s
`assert_eq!(MIGRATIONS.len(), 85)` → `86`, and `:1789`'s name list.

---

## 6. `--init` Wizard (`src/init/agents.rs::step_orchestration`)

Add one yes/no prompt after the existing `confirm_before_execute` question:

```rust
state.orchestration_interrupt_enabled = prompt_bool(
    "Enable human-in-the-loop interrupt gates for plan tasks? (advanced; requires an \
     interactive operator to answer `/plan provide` — leave off for headless/gateway use)",
    false, // default No
)?;
```

Add `orchestration_interrupt_enabled: bool` to the wizard `State` struct (`src/init/mod.rs:~113`,
next to the other `orchestration_*` fields) and thread it into the final `OrchestrationConfig`
struct-literal assembly (`src/init/mod.rs:1099-1114`).

---

## 7. TUI

No dedicated TUI palette work needed beyond the existing generic `/plan <subcommand>` slash-command
autocomplete (`030-tui-slash-autocomplete/spec.md` reuses the shared `filter_commands` registry —
registering `Provide` in `zeph-commands` per §3.3 is sufficient; the autocomplete dropdown picks it
up automatically, same as `resume`/`retry` did).

---

## 8. Testing Strategy

| Level | What | Notes |
|-------|------|-------|
| Unit | `graph.rs`: serde round-trip for `InterruptRequest`, `PauseReason`, new `TaskNode`/`TaskGraph` fields; old-blob backward-compat deserialization (extends the existing test block at `graph.rs:989-1065`) | Mirrors `network_scope`/`asset_sensitivity` test pattern exactly |
| Unit | `dag.rs` / `scheduler/tick/mod.rs`: gate fires on `Ready` + `interrupt_before.is_some()` + `resolved_input.is_none()` + `interrupt_enabled`; does not fire when any condition is false; earlier same-tick dispatches are kept (FR-003) | New tests |
| Unit | `scheduler/planner.rs`: **regression test that `check_graph_completion` does not flip to `Failed`+cancel-all** when the interrupt-gated task is the sole ready task with zero running siblings (§3.4 step 2 / spec.md §5) | This is the highest-value single test in this PR — construct a 1-task graph, set `interrupt_before`, tick once, assert `graph.status == Paused` (not `Failed`) and all other tasks (none, in the minimal case; add a second independent task in a fuller variant) remain non-`Canceled` |
| Unit | `scheduler/router.rs`: `build_task_prompt` interpolates `resolved_input` (both `Value::String` and structured-JSON cases) | New tests |
| Unit | `plan.rs`: `handle_plan_provide_as_string` — success path, no-active-graph rejection, wrong-pause-reason rejection, JSON-parse-fallback-to-string | New tests |
| Unit | `plan.rs`: `handle_plan_retry_as_string` rejects retry on `AwaitingInput` pause (FR-009) | New test |
| Integration | Full pause → `/plan provide` → `/plan confirm` → dispatch cycle against an in-memory `DagScheduler`, asserting the dispatched prompt contains the interpolated value | New test, `zeph-orchestration` integration-style unit test |
| Integration | Crash-resume: persist a graph with `pause_reason = Some(AwaitingInput{..})`, reload via `GraphPersistence::load`, assert fields intact | Extends existing `GraphPersistence` round-trip tests |
| Live testing | See `.local/testing/playbooks/orchestration-hitl-interrupt.md` (tasks.md T-014) | Manual/CI-cycle coverage, per CLAUDE.md Development Rules point 6 |

Run: `cargo nextest run --config-file .github/nextest.toml --workspace --features
"desktop,ide,server,chat,pdf,scheduler,testing" --lib --bins` plus the doc-test gate for
`zeph-orchestration` (`cargo test --doc -p zeph-orchestration`).

---

## 9. Security

- Resolution path is command-handler-only (§3 of this plan); no LLM/sub-agent code path can reach
  `resolved_input` or `pause_reason` mutation — verified structurally, not by runtime check (there
  is no such call site to guard).
- `interrupt_before` excluded from `PlannedTask`/`PlannerResponse` (FR-014) — add a unit test
  asserting the `schemars::JsonSchema`-derived schema for `PlannerResponse` does not contain an
  `interrupt_before` property, as a compile-time-adjacent regression guard.
- `resolved_input` is plain-text/JSON interpolated into a sub-agent prompt — it is operator-
  supplied, not attacker-controlled in the threat model this spec addresses (the operator is
  trusted, same trust level as anyone issuing `/plan` commands today). No new sanitizer bypass is
  introduced; the existing sanitizer pipeline still processes the sub-agent's *output*, unaffected
  by this input-side change.

---

## 10. Constitution / CLAUDE.md Compliance

| Requirement | Status |
|---|---|
| Multi-Model Design (`*_provider` field) | N/A — no LLM call added by this feature |
| Async & Background Tasks (no new `tokio::spawn`) | Compliant — verified by scan (spec.md §3.7, §6 Success Criteria) |
| Development Rules — 6 integration points | config.toml (§4) ✓, CLI/command (§3) ✓, TUI (§7) ✓, `--init` (§6) ✓, `--migrate-config` (§5) ✓, playbook + coverage-status rows (tasks.md T-014/T-015) ✓ |
| TUI status indicator for background ops | N/A — this feature adds no new background/implicit operation; pause-and-return is synchronous within the existing tick loop, already covered by the existing plan-execution status messaging |

---

## 11. Risks and Mitigations

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| `check_graph_completion` false-deadlock (§2.3) | High — spurious `Failed` + cancel-all on a legitimate pause | Low (the invariant is simple to hold: never mutate `task.status` in the gate) but easy to violate in a future refactor | Dedicated regression test (T-009), explicit invariant callout in spec.md §4 and this plan §2.3 |
| Operator answers with malformed JSON they intended as JSON (e.g., trailing comma) | Low — silently treated as a plain string instead of erroring | Medium | Acceptable per FR-005's explicit fallback design; document the behavior in the playbook so live-testers know it's intentional, not a bug |
| Headless/gateway-triggered plans with `interrupt_enabled = true` and no operator present | Medium — plan hangs paused indefinitely (by design, C3) | Low (default is `false`) | Default-off config (§4), `--init` wizard warns "requires an interactive operator" (§6) |
