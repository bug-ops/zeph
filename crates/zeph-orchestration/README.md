# zeph-orchestration

[![Crates.io](https://img.shields.io/crates/v/zeph-orchestration)](https://crates.io/crates/zeph-orchestration)
[![docs.rs](https://img.shields.io/docsrs/zeph-orchestration)](https://docs.rs/zeph-orchestration)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

DAG-based task orchestration with failure propagation, LLM planning, and SQLite persistence for Zeph.

## Overview

Implements the multi-agent task orchestration pipeline extracted from `zeph-core`. Decomposes high-level goals into directed acyclic graphs of sub-tasks, executes them via a tick-based scheduler, routes tasks to sub-agents, aggregates results through a final LLM synthesis call, and persists graph state in SQLite for resume and retry. Includes plan template caching for repeated goals.

## Key modules

| Module | Description |
|--------|-------------|
| `graph` | `TaskGraph`, `TaskNode`, `TaskId`, `GraphId` typed identifiers; `TaskStatus`, `GraphStatus`, `FailureStrategy` (abort/retry/skip/ask) |
| `dag` | DAG validation (cycle detection via topological sort), `ready_tasks`, `propagate_failure`, `reset_for_retry` |
| `scheduler` | `DagScheduler` tick-based execution engine; `SchedulerAction` command pattern; `TaskEvent`, `TaskOutcome` |
| `planner` | `Planner` trait + `LlmPlanner` — goal decomposition via `chat_typed` structured output; maps string task IDs to `TaskId` |
| `aggregator` | `Aggregator` trait + `LlmAggregator` — synthesizes completed task outputs; per-task character budget; content-sanitized before injection |
| `router` | `AgentRouter` trait + `RuleBasedRouter` — 3-step fallback task-to-agent routing |
| `plan_cache` | `PlanCache` — caches plan templates by normalized goal hash; `PlanTemplate` captures task structure from a `TaskGraph` for reuse; `normalize_goal` + `goal_hash` for deterministic cache keys |
| `command` | `PlanCommand` parser for `/plan` CLI slash commands |
| `error` | `OrchestrationError` unified error type |

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
planner_max_tokens = 4096          # LLM token budget for goal decomposition
dependency_context_budget = 16384  # chars of cross-task context injected per task
confirm_before_execute = true      # require /plan confirm before starting
aggregator_max_tokens = 4096       # token budget for LlmAggregator synthesis call
```

## Failure strategies

| Strategy | Behavior when a task fails |
|----------|---------------------------|
| `Abort` | Cancel all remaining tasks and mark the graph failed |
| `Retry` | Re-queue the failed task up to `max_retries` times |
| `Skip` | Mark the task skipped and continue with dependents |
| `Ask` | Pause the graph and wait for `/plan resume` from the user |

## Plan template caching

When a goal is decomposed into a task graph, the resulting structure is cached as a `PlanTemplate` keyed by a normalized goal hash. Subsequent requests with semantically equivalent goals reuse the cached template instead of invoking the LLM planner, reducing latency and token costs for repeated orchestration patterns.

## Integration points

- `zeph-core` integrates `DagScheduler` and `LlmPlanner` into the agent loop via the `orchestration` module
- `zeph-memory::RawGraphStore` / `SqliteGraphStore` persists graph state
- `zeph-sanitizer::ContentSanitizer` wraps cross-task context before injection
- `zeph-subagent::SubAgentManager::spawn_for_task()` spawns sub-agents per task

## Installation

```bash
cargo add zeph-orchestration
```

Enabled via the `orchestration` feature flag on the root `zeph` crate.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
