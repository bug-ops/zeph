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
| `command` | `PlanCommand` parser for `/plan` CLI slash commands |
| `error` | `OrchestrationError` unified error type |
| `planner` | `Planner` trait + `LlmPlanner` — goal decomposition via `chat_typed` structured output (feature `llm-planning`) |
| `aggregator` | `Aggregator` trait + `LlmAggregator` — synthesizes completed task outputs; content-sanitized before injection (feature `llm-planning`) |
| `verifier` | `PlanVerifier` — post-task and whole-plan completeness verifier with targeted replan, grounded against the DAG-wide tool-call trace (feature `llm-planning`) |
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
# planner_provider = "quality"     # provider name from [[llm.providers]] for planning; empty = primary provider
planner_max_tokens = 4096          # LLM token budget for goal decomposition
dependency_context_budget = 16384  # chars of cross-task context injected per task
confirm_before_execute = true      # require /plan confirm before starting
aggregator_max_tokens = 4096       # token budget for LlmAggregator synthesis call
```

Deterministic ensemble-merge verification (opt-in, disabled by default) has its own `[orchestration.ensemble]` table — see the `ensemble` module above.

## Failure strategies

| Strategy | Behavior when a task fails |
|----------|---------------------------|
| `Abort` | Cancel all remaining tasks and mark the graph failed |
| `Retry` | Re-queue the failed task up to `max_retries` times |
| `Skip` | Mark the task skipped and continue with dependents |
| `Ask` | Pause the graph and wait for `/plan resume` from the user |

> [!NOTE]
> A `TaskNode` can also carry a declarative `recovery: RecoveryAction` (Mode 1, spec `075-orchestration-node-control-parity`). On an `Abort`-default or retry-exhausted `Retry` failure, a node with `recovery` set is marked `Completed` with `state_injection` substituted as its output instead of failing the graph, letting downstream tasks proceed. `run_timeout_secs` on the same node bounds both spawned and `RunInline` dispatch; `None` falls back to `OrchestrationConfig::task_timeout_secs`.

## Plan template caching

When a goal is decomposed into a task graph, the resulting structure is cached as a `PlanTemplate` keyed by a normalized goal hash. Subsequent requests with semantically equivalent goals reuse the cached template instead of invoking the LLM planner, reducing latency and token costs for repeated orchestration patterns.

## Integration points

- `zeph-core` integrates `DagScheduler` and `LlmPlanner` into the agent loop via the `orchestration` module
- `zeph-memory::RawGraphStore` / `TaskGraphStore` persists graph state
- `zeph-sanitizer::ContentSanitizer` wraps cross-task context before injection
- `zeph-subagent::SubAgentManager::spawn_for_task()` spawns sub-agents per task

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
