# zeph-orchestration

[![Crates.io](https://img.shields.io/crates/v/zeph-orchestration)](https://crates.io/crates/zeph-orchestration)
[![docs.rs](https://img.shields.io/docsrs/zeph-orchestration)](https://docs.rs/zeph-orchestration)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

DAG-based task orchestration with failure propagation, LLM planning, and SQLite persistence for Zeph.

## Overview

Implements the multi-agent task orchestration pipeline extracted from `zeph-core`. Decomposes high-level goals into directed acyclic graphs of sub-tasks, executes them via a tick-based scheduler, routes tasks to sub-agents, aggregates results through a final LLM synthesis call, and persists graph state in SQLite for resume and retry. Includes plan template caching for repeated goals.

## Key modules

| Module | Description |
|--------|-------------|
| `graph` | `TaskGraph`, `TaskNode`, `TaskId`, `GraphId` typed identifiers; `TaskStatus`, `GraphStatus`, `FailureStrategy` (abort/retry/skip/ask); per-task `TimeoutPolicy` (`run_timeout_secs`) and Mode-1 `RecoveryAction` (`state_injection`) |
| `dag` | DAG validation (cycle detection via topological sort), `ready_tasks`, `propagate_failure`, `reset_for_retry` |
| `scheduler` | `DagScheduler` tick-based execution engine; `SchedulerAction` command pattern; `TaskEvent`, `TaskOutcome` |
| `topology` | `TopologyClassifier`, `Topology`, `DispatchStrategy` — DAG shape analysis for dispatch selection |
| `cascade` | `CascadeDetector`, `CascadeConfig`, `RegionHealth` — cascading-failure detection and abort decisions |
| `lineage` | `ErrorLineage`, `classify_error` — error provenance tracking across the DAG |
| `router` | `AgentRouter` trait + `RuleBasedRouter` — 3-step fallback task-to-agent routing |
| `admission` | `AdmissionGate` — per-provider `Semaphore` admission control bounding how many sub-agents may be dispatched concurrently against one LLM provider, preventing 429 cascades and cost spikes |
| `durable` | `ReplanBudgetSnapshot`, `journal_budget`/`restore_budget` — journals the replan budget to `zeph-durable` on pause and restores it on `/plan resume` (one execution per save generation, so a stale snapshot is never replayed) |
| `command` | `PlanCommand` parser for `/plan` CLI slash commands; `HandoffCommand`/`parse_handoff_command`/`has_handoff_fence` — parses a node's trailing ` ```zeph-command ` fenced JSON block for Command-style dynamic task handoff (spec-080, feature `[orchestration.command]`) |
| `error` | `OrchestrationError` unified error type |
| `planner` | `Planner` trait + `LlmPlanner` — goal decomposition via `chat_typed` structured output (feature `llm-planning`) |
| `aggregator` | `Aggregator` trait + `LlmAggregator` — synthesizes completed task outputs; content-sanitized before injection (feature `llm-planning`) |
| `verifier` | `PlanVerifier` — post-task and whole-plan completeness verifier with targeted replan, grounded against the DAG-wide tool-call trace (feature `llm-planning`) |
| `verify_predicate` | `PredicateEvaluator` — evaluates a node's optional `VerifyPredicate` and records a `PredicateOutcome`, driving predicate-scoped reruns (feature `llm-planning`) |
| `ensemble` | `EnsembleVerifier`, `EnsembleTracker` — N-fold parallel dispatch of `PlanVerifier` gap-severity checks across configured providers with deterministic majority-vote merge (spec `073-orch-ensemble-merge`, `[orchestration.ensemble]`, feature `llm-planning`) |
| `plan_cache` | `PlanCache` — caches plan templates by normalized goal hash; `normalize_goal` + `goal_hash` for deterministic cache keys (feature `llm-planning`) |
| `adaptorch` | `TopologyAdvisor` — adaptive topology hints for the scheduler (feature `llm-planning`) |

## Usage

Orchestration is triggered via `/plan` commands in the agent chat:

```text
/plan analyze the codebase and write a test report
/plan confirm           # confirm and start execution
/plan status            # show DAG progress
/plan list              # list recent graphs
/plan cancel            # cancel active graph
/plan retry             # re-queue failed tasks
/plan resume            # resume a paused graph (Ask failure strategy)
```

> [!NOTE]
> When `confirm_before_execute = true` (default), `/plan <goal>` creates the graph and pauses for confirmation. Run `/plan confirm` to start execution or `/plan cancel` to discard.

## Configuration

```toml
[orchestration]
enabled = false                    # master switch: orchestration is opt-in
max_tasks = 20                     # maximum nodes a single plan may contain
max_parallel = 4                   # maximum tasks dispatched concurrently
task_timeout_secs = 300            # per-task fallback when a node sets no TimeoutPolicy
default_failure_strategy = "abort" # "abort" | "retry" | "skip" | "ask"
default_max_retries = 3            # retry budget for the "retry" strategy
# planner_provider = "quality"     # provider name from [[llm.providers]] for planning; empty = primary provider
planner_max_tokens = 4096          # LLM token budget for goal decomposition
dependency_context_budget = 16384  # chars of cross-task context injected per task
confirm_before_execute = true      # require /plan confirm before starting
aggregator_max_tokens = 4096       # token budget for LlmAggregator synthesis call
whole_plan_verifier_timeout_secs = 0  # timeout for the whole-plan verify_plan() call; 0 = fall back to verifier_timeout_secs

[orchestration.command]
enabled      = false   # opt-in: node-agent-driven dynamic task handoff via a zeph-command block
max_handoffs = 16      # per-graph livelock budget on Command.goto handoffs
```

Deterministic ensemble-merge verification (opt-in, disabled by default) has its own `[orchestration.ensemble]` table — see the `ensemble` module above.

## Command-style dynamic task handoff

Opt-in LangGraph-`Command`-style routing (spec-080, `[orchestration.command]`, default disabled): a DAG node's agent can end its output with a trailing fenced block naming an alternate target task and a state update, instead of following the statically declared `depends_on` graph:

````text
Investigated the failure.
```zeph-command
{"goto": "2", "update": {"finding": "root cause found"}}
```
````

`dag::try_handoff` resolves `goto` against the graph (by `TaskId` or unique title — an ambiguous or absent match is rejected as `OrchestrationError::InvalidHandoffTarget`, never guessed), activates the target forward-only within `max_handoffs`, and persists `update` into the cross-thread key-value store (`zeph-memory`, `[memory.store]`, exposed via `zeph store` CLI / `/store` slash command in `zeph-commands`) before the routed-to node's prompt is built. `zeph-orchestration` has no production dependency on `zeph-memory` — all store I/O is performed by the `zeph-core` binding layer, per the design's layering invariant. The parsed command is scanned through the sanitizer before it can drive routing or a store write.

## Failure strategies

| Strategy | Behavior when a task fails |
|----------|---------------------------|
| `Abort` | Cancel all remaining tasks and mark the graph failed |
| `Retry` | Re-queue the failed task up to `max_retries` times |
| `Skip` | Mark the task skipped and transitively skip all of its dependents |
| `Ask` | Pause the graph and wait for `/plan resume` from the user |

> [!NOTE]
> A `TaskNode` can also carry a declarative `recovery: Option<RecoveryAction>` (spec `075-orchestration-node-control-parity`) with two mutually exclusive modes — `dag::validate` rejects a node that sets both. Both fire on the same conditions: an `Abort`-default or retry-exhausted `Retry` failure.
>
> - **Mode 1 — `recovery.state_injection: Option<String>`**: the node is marked `Completed` with the given string substituted as its `TaskResult` output instead of failing the graph, letting downstream tasks proceed.
> - **Mode 2 — `recovery.route_to: Option<TaskId>`**: the target node is activated as an alternate fallback branch (and the failed node's output is injected into its prompt) instead of substituting inline state. The fallback starts in `TaskStatus::Dormant` — excluded from normal scheduling — until `dag::mark_dormant_route_to_targets` activates it on failure. `dag::validate` requires the target to have an empty `depends_on` and to not itself set `route_to`; v1 does not support chained reroutes.
>
> Separately, a node's `timeout: Option<TimeoutPolicy>` carries `run_timeout_secs` (bounds both spawned and `RunInline` dispatch) and `idle_timeout_secs`. `None` falls back to `OrchestrationConfig::task_timeout_secs`.

## Plan template caching

When a goal is decomposed into a task graph, the resulting structure is cached as a `PlanTemplate` keyed by a normalized goal hash. Subsequent requests with semantically equivalent goals reuse the cached template instead of invoking the LLM planner, reducing latency and token costs for repeated orchestration patterns.

## Integration points

- `zeph-core` integrates `DagScheduler` and `LlmPlanner` into the agent loop via the `orchestration` module
- `zeph-memory::RawGraphStore` / `TaskGraphStore` persists graph state
- `zeph-sanitizer::ContentSanitizer` wraps cross-task context before injection
- `zeph-subagent::SubAgentManager::spawn_for_task()` spawns sub-agents per task
- `zeph-memory`'s cross-thread key-value store backs Command-style handoff state updates — accessed only through `zeph-core`, never directly by this crate

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend for graph persistence (via `zeph-db`, `zeph-durable`, `zeph-memory`, `zeph-subagent`) |
| `postgres` | no | PostgreSQL backend |
| `llm-planning` | no | Enables the LLM-dependent modules (`planner`, `aggregator`, `verifier`, `verify_predicate`, `plan_cache`, `adaptorch`, `ensemble`) and the `zeph-llm` dependency |
| `test-utils` | no | Testcontainers for PostgreSQL integration tests (implies `postgres` and `llm-planning`) |

> [!NOTE]
> Without `llm-planning`, the crate builds as a pure-DAG scheduler subset with no `zeph-llm` dependency — useful for embedding the scheduler without an LLM backend.

## Installation

```bash
cargo add zeph-orchestration

# With LLM-backed planning and aggregation
cargo add zeph-orchestration --features llm-planning
```

Enabled via the `orchestration` feature flag on the root `zeph` crate.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
