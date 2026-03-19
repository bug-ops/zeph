# zeph-orchestration

Task orchestration for Zeph: DAG execution, failure propagation, and persistence.

Extracted from `zeph-core` as part of the epic-1973 architecture refactoring (Phase 1g).

## Modules

- `graph` — `TaskGraph`, `TaskNode`, `TaskId`, `GraphId`, `TaskStatus`, `GraphStatus`, `FailureStrategy`, `GraphPersistence`
- `dag` — DAG validation, topological sort, `ready_tasks`, `propagate_failure`, `reset_for_retry`
- `scheduler` — `DagScheduler` (tick-based execution engine), `SchedulerAction`, `TaskEvent`, `TaskOutcome`
- `planner` — `Planner` trait + `LlmPlanner` (structured LLM output goal decomposition)
- `aggregator` — `Aggregator` trait + `LlmAggregator` (per-task token budget + fallback)
- `router` — `AgentRouter` trait + `RuleBasedRouter` (3-step fallback routing)
- `command` — `PlanCommand` parser for `/plan` CLI commands
- `error` — `OrchestrationError`

## Dependencies

Layer 2 crate. Depends on:
- `zeph-config` — `OrchestrationConfig`
- `zeph-common` — `truncate_chars`
- `zeph-llm` — `LlmProvider`, `Message`, `Role`
- `zeph-memory` — `RawGraphStore`, `GraphSummary`
- `zeph-sanitizer` — `ContentSanitizer`
- `zeph-subagent` — `SubAgentDef`, `SubAgentError`, `ToolPolicy`
