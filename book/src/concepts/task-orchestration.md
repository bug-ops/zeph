# Task Orchestration

Use task orchestration to break a complex goal into a directed acyclic graph (DAG) of dependent tasks, execute them in parallel where possible, and recover from failures without restarting the entire plan. This page explains the core types, DAG algorithms, scheduling model, result aggregation, and the `/plan` CLI commands.

Task orchestration persists graph state in SQLite so execution survives restarts.

## Core Types

### TaskGraph

A `TaskGraph` represents a plan: a goal string, a list of `TaskNode` entries, and graph-level defaults for failure handling. Each graph has a UUID-based `GraphId` and tracks its lifecycle through `GraphStatus`.

| Status | Description |
|--------|-------------|
| `created` | Graph has been built but not yet started |
| `running` | At least one task is executing |
| `completed` | All tasks finished successfully |
| `failed` | A task failed and the failure strategy aborted the graph |
| `canceled` | The graph was canceled externally |
| `paused` | A task failed with the `ask` strategy; awaiting user input |

### TaskNode

Each node in the DAG carries a `TaskId` (zero-based index), a title, a description, dependency edges, and an optional agent hint for sub-agent routing. Nodes progress through `TaskStatus`:

| Status | Terminal? | Description |
|--------|-----------|-------------|
| `pending` | no | Waiting for dependencies |
| `ready` | no | All dependencies completed; eligible for scheduling |
| `running` | no | Currently executing |
| `completed` | yes | Finished successfully |
| `failed` | yes | Execution failed |
| `skipped` | yes | Skipped due to a dependency failure |
| `canceled` | yes | Canceled externally or by abort propagation |

### TaskResult

When a task completes, it produces a `TaskResult` containing:

- `output` — text output from the task
- `artifacts` — file paths produced by the task
- `duration_ms` — wall-clock execution time
- `agent_id` / `agent_def` — which sub-agent executed the task (optional)

## DAG Algorithms

The orchestration module provides four core algorithms:

### validate

Checks structural integrity before execution begins:

- Task count does not exceed `max_tasks`.
- At least one task exists.
- `tasks[i].id == TaskId(i)` invariant holds.
- No self-references or dangling dependency edges.
- No cycles (verified via topological sort).
- At least one root node (no dependencies).

### toposort

Kahn's algorithm producing dependency order (roots first). Used internally by `validate` and available for scheduling.

### ready_tasks

Returns all tasks eligible for scheduling: tasks already in `Ready` status, plus `Pending` tasks whose dependencies have all reached `Completed`. The function is idempotent across scheduler ticks.

### propagate_failure

Applies the effective failure strategy when a task fails:

| Strategy | Behavior |
|----------|----------|
| `abort` | Set graph status to `Failed`; return all `Running` task IDs for cancellation |
| `skip` | Mark the failed task and all transitive dependents as `Skipped` via BFS |
| `retry` | Increment retry counter and reset to `Ready` if under `max_retries`; otherwise fall through to `abort` |
| `ask` | Set graph status to `Paused`; await user decision |

Each task can override the graph-level default strategy via its `failure_strategy` and `max_retries` fields.

## Persistence

Graph state is persisted to the `task_graphs` SQLite table (migration `022_task_graphs.sql`). The `GraphPersistence` wrapper serializes `TaskGraph` to JSON for storage and provides CRUD operations:

| Operation | Description |
|-----------|-------------|
| `save` | Upsert a graph (rejects goals longer than 1024 characters) |
| `load` | Retrieve a graph by `GraphId` |
| `list` | List stored graphs, newest first |
| `delete` | Remove a graph by `GraphId` |

The `RawGraphStore` trait abstracts the storage backend; `SqliteGraphStore` in `zeph-memory` is the default implementation.

## LLM Planner

The LLM planner performs goal decomposition: it takes a high-level user goal and breaks it into a validated `TaskGraph` via a single LLM call with structured JSON output.

### Planning Flow

1. The user provides a natural-language goal (e.g., "build and deploy the staging environment").
2. The planner builds a prompt containing the goal, the available agent catalog, and formatting rules.
3. The LLM returns a JSON object with a `tasks` array. Each task specifies a `task_id`, `title`, `description`, optional `depends_on` edges, an optional `agent_hint`, and an optional `failure_strategy`.
4. The response is parsed and validated: task IDs must be unique kebab-case strings (`^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`), dependency references must resolve, and the total task count must not exceed `max_tasks`.
5. String `task_id` values from the LLM output are mapped to internal `TaskId(u32)` indices based on array position.
6. The resulting `TaskGraph` is checked for DAG acyclicity via `dag::validate`.

If the LLM returns malformed JSON, `chat_typed` retries the call once before propagating the error as `OrchestrationError::PlanningFailed`.

### Agent Catalog

The planner receives the list of available `SubAgentDef` entries and includes each agent's name, description, and tool policy in the system prompt. This allows the LLM to assign an `agent_hint` to each task, routing it to the most appropriate agent. Unknown agent hints are logged as warnings and silently dropped rather than failing the plan.

### Configuration Fields

Two config fields control planner behavior:

- `planner_model` — override the model used for planning LLM calls. When unset, the caller provides whatever provider is configured for the agent. Currently reserved for future caller-side provider selection; `LlmPlanner` uses the provider it receives at construction time.
- `planner_max_tokens` — maximum tokens for the planner LLM response (default: 4096). Currently reserved for future use: the underlying `chat_typed` API does not yet support per-call token limits.

See [Configuration](../reference/configuration.md) for the full `[orchestration]` section reference.

## Execution

Once a `TaskGraph` is validated and persisted, the **DAG scheduler** drives execution by producing actions for the caller to perform.

### DagScheduler

`DagScheduler` implements a tick-based execution loop. On each tick it inspects the graph, checks for ready tasks, monitors timeouts, and emits `SchedulerAction` values:

| Action | Description |
|--------|-------------|
| `Spawn` | Spawn a sub-agent for a ready task (includes task ID, agent definition name, and prompt) |
| `RunInline` | Execute the task prompt directly on the main agent provider when no sub-agents are configured |
| `Cancel` | Cancel a running sub-agent (on graph abort or skip propagation) |
| `Done` | Graph reached a terminal or paused state |

The scheduler never holds a mutable reference to `SubAgentManager` — it produces actions for the caller to execute (command pattern). This keeps the scheduler testable in isolation and avoids borrow conflicts.

#### Concurrency Backoff

When all ready tasks are deferred because `max_parallel` concurrency slots are full, `wait_event()` applies exponential backoff instead of spinning: 250ms → 500ms → 1s → 2s → 4s, capped at 5s. The backoff resets to 250ms as soon as the first task successfully spawns. This eliminates CPU spin-loops and log floods under sustained high concurrency.

When the sub-agent manager rejects a spawn with a `ConcurrencyLimit` error, the affected task is reverted to `Ready` instead of being marked `Failed`, preventing spurious failure cascades.

#### Event Channel

Sub-agents report completion via an `mpsc::Sender<TaskEvent>` channel. Each `TaskEvent` carries the task ID, agent handle ID, and an outcome (`Completed` with output/artifacts, or `Failed` with an error message). The scheduler buffers events in a `VecDeque` between `wait_event()` and `tick()` calls.

A **stale event guard** rejects completion events from agents that were timed out and retried — preventing a late response from a previous attempt from overwriting the retry result.

#### Task Timeout

The scheduler monitors wall-clock time for each running task against `task_timeout_secs`. When a task exceeds the timeout, the scheduler marks it as failed with a timeout error and applies the configured failure strategy (retry, abort, skip, or ask).

#### Cross-Task Context Injection

When a task becomes ready, the scheduler collects output from its completed dependencies and injects it into the task prompt as a `<completed-dependencies>` XML block. This gives downstream tasks access to upstream results without manual plumbing.

The injection respects `dependency_context_budget` (total character budget across all dependencies). Output is truncated at character-safe boundaries (no mid-codepoint splits). The `ContentSanitizer` is applied to dependency output before injection to prevent prompt injection from upstream task results.

### Agent Router

The `AgentRouter` trait selects which sub-agent definition to use for a given task. The built-in `RuleBasedRouter` implements a 3-step fallback chain:

1. **Exact match** — `task.agent_hint` matched against available agent names.
2. **Tool keyword matching** — keywords in the task description (e.g., "implement", "edit", "build") matched against agent tool policies. This is an MVP heuristic (English-only, last resort).
3. **First available** — unconditional fallback to the first agent in the list.

For reliable routing, set `agent_hint` on each task node during planning. The keyword matching step is a best-effort fallback, not authoritative routing.

#### Inline Execution (Single-Agent Setup)

When no sub-agents are configured, the scheduler emits `RunInline` instead of marking tasks as `Failed`. The main agent provider executes the task prompt directly. This means `/plan` works in single-agent setups without requiring any `[agents]` configuration.

### SubAgentManager Integration

`SubAgentManager::spawn_for_task()` wraps the standard `spawn()` method and hooks into the scheduler's event channel. When the sub-agent's `JoinHandle` resolves, it automatically sends a `TaskEvent` to the scheduler. This is minimally invasive — no changes to `SubAgentHandle` or `run_agent_loop` internals.

## Result Aggregation

When all tasks in a graph reach a terminal state (completed, skipped, or failed), the orchestrator synthesizes a single coherent response via the `Aggregator` trait.

### LlmAggregator

`LlmAggregator` is the default implementation. It:

1. Collects all `Completed` task outputs.
2. Truncates each output to a per-task character budget derived from `aggregator_max_tokens` (budget = `aggregator_max_tokens × 4` characters, divided equally across completed tasks).
3. Applies the `ContentSanitizer` to each output to guard against prompt injection from task results.
4. Builds a synthesis prompt listing task outputs under `### Task: <title>` headers. Skipped tasks are listed separately with a note that their output is absent.
5. Calls the LLM to produce a single summary that directly addresses the original goal.

**Fallback behavior:** if the LLM call fails for any reason, `LlmAggregator` falls back to raw concatenation — goal header followed by each task's output verbatim. The call never fails with an error as long as at least one completed or skipped task exists.

> [!NOTE]
> If the graph has no completed or skipped tasks at all (e.g., every task failed before producing output), aggregation returns `OrchestrationError::AggregationFailed`.

## TUI Integration

When running with the TUI dashboard (`--features tui`), the right side panel provides live plan progress without leaving the interface.

Press `p` in Normal mode to toggle between the Sub-agents view and the Plan View. The panel shows each task with its current status, assigned agent, elapsed time, and any error message:

```text
+--------------------+
| Plan: deploy stag… |
| ↻ Preparing env    |  Running   agent-1   12s
| ✓ Build image      |  Completed agent-2   45s
| ✗ Push artifact    |  Failed    agent-2   8s   image push timeout
| · Run smoke tests  |  Pending   —         —
+--------------------+
```

Use `plan:confirm`, `plan:cancel`, `plan:status`, and `plan:list` from the command palette (`Ctrl+P`) instead of typing `/plan …` in the input line.

See [TUI Dashboard — Plan View](../advanced/tui.md#plan-view) for the full keybinding and color reference.

## CLI Commands

| Command | Description |
|---------|-------------|
| `/plan <goal>` | Decompose goal into a DAG, show confirmation, then execute |
| `/plan confirm` | Confirm and execute the pending plan |
| `/plan status` | Show current graph progress |
| `/plan status <id>` | Show a specific graph by UUID |
| `/plan list` | List recent graphs from persistence |
| `/plan cancel` | Cancel the active graph |
| `/plan cancel <id>` | Cancel a specific graph by UUID |
| `/plan resume` | Resume the active paused graph (`ask` failure strategy) |
| `/plan resume <id>` | Resume a specific paused graph by UUID |
| `/plan retry` | Re-run failed tasks in the active graph |
| `/plan retry <id>` | Re-run failed tasks in a specific graph by UUID |

> [!NOTE]
> **Parsing ambiguity:** goals that begin with a reserved subcommand name (`status`, `list`, `cancel`, `confirm`, `resume`, `retry`) are interpreted as that subcommand. Rephrase the goal to avoid collisions — e.g., `/plan write a status report` instead of `/plan status report`.

### Confirmation Flow

When `confirm_before_execute` is enabled (the default), `/plan <goal>` does not execute immediately. Instead it:

1. Calls the LLM planner to decompose the goal into a `TaskGraph`.
2. Displays a summary of planned tasks with agent assignments.
3. Stores the graph in a pending state.

The user then runs `/plan confirm` to start execution, or `/plan cancel` to discard the pending plan. If a new `/plan <goal>` is submitted while a plan is already pending, the agent rejects it with a warning — cancel or confirm the existing plan first.

### Canceling a Running Plan

`/plan cancel` is delivered even during active plan execution. The agent loop polls the input channel concurrently with the scheduler's event wait (`tokio::select!`). When `/plan cancel` arrives mid-execution, it calls `cancel_all()` on the scheduler, aborts all running sub-agent tasks, and exits the scheduler loop with a `Canceled` graph status. Messages received during execution that are not cancel commands are queued and processed after the plan finishes.

### Resume a Paused Graph

A graph enters the `paused` state when a task fails and the effective failure strategy is `ask`. This gives the user a chance to decide how to proceed.

Use `/plan resume` (or `/plan resume <id>` for a specific graph) to continue execution. The scheduler re-evaluates ready tasks from the current state — no previously completed task is re-run.

**When to use:** the `ask` strategy is useful when a task failure may or may not be critical. Configure it per-task in the planner output or as the graph-level `default_failure_strategy`.

### Retry Failed Tasks

Use `/plan retry` (or `/plan retry <id>` for a specific graph) to re-attempt all tasks that did not complete successfully:

- Tasks in `Failed` status are reset to `Ready`; their `assigned_agent` field is cleared to prevent scheduler deadlock on a stale assignment.
- Tasks in `Skipped` status are reset to `Pending` so they can be re-evaluated once their dependencies succeed.
- Tasks that already `Completed` are not re-run.

This is equivalent to a targeted re-run of the failed subtree without discarding the entire plan.

## Metrics

`OrchestrationMetrics` tracks plan and task counters. The struct is always present in `MetricsSnapshot` and defaults to zero when orchestration is inactive.

| Field | Type | Description |
|-------|------|-------------|
| `plans_total` | `u64` | Total plans created |
| `tasks_total` | `u64` | Total tasks across all plans |
| `tasks_completed` | `u64` | Tasks that finished successfully |
| `tasks_failed` | `u64` | Tasks that failed after all retries |
| `tasks_skipped` | `u64` | Tasks skipped due to dependency failures |

Metrics are updated in the agent loop as tasks progress. They are available through the same `watch` channel that feeds the TUI dashboard.

## Configuration

Add an `[orchestration]` section to `config.toml`:

```toml
[orchestration]
enabled = true
max_tasks = 20                      # Maximum tasks per graph (default: 20)
max_parallel = 4                    # Maximum concurrent task executions (default: 4)
default_failure_strategy = "abort"  # abort, retry, skip, or ask (default: "abort")
default_max_retries = 3             # Retries for the "retry" strategy (default: 3)
task_timeout_secs = 300             # Per-task timeout in seconds, 0 = fallback to 600s (default: 300)
# planner_model = "claude-sonnet-4-20250514"  # Model override for planning LLM calls
planner_max_tokens = 4096           # Max tokens for planner response (default: 4096; reserved)
dependency_context_budget = 16384   # Character budget for cross-task context (default: 16384)
confirm_before_execute = true       # Show confirmation before executing a plan (default: true)
aggregator_max_tokens = 4096        # Token budget for the aggregation LLM call (default: 4096)

[orchestration.plan_cache]
enabled = false                     # Enable plan template caching (default: false)
similarity_threshold = 0.90         # Min cosine similarity for cache hit (default: 0.90)
ttl_days = 30                       # Days since last access before eviction (default: 30)
max_templates = 100                  # Maximum cached templates (default: 100)
```

## Plan Template Caching

When `[orchestration.plan_cache]` is enabled, successful plan decompositions are cached as templates. On subsequent `/plan` invocations, the planner first searches for a cached template with cosine similarity above `similarity_threshold` (default: 0.90). If a match is found, the cached task graph structure is reused — skipping the LLM planning call entirely.

```toml
[orchestration.plan_cache]
enabled = true                # Enable plan template caching (default: false)
similarity_threshold = 0.90   # Min cosine similarity for a cache hit (default: 0.90)
ttl_days = 30                 # Days since last access before eviction (default: 30)
max_templates = 100            # Maximum cached templates (default: 100)
```

Templates are stored in SQLite (migration `040_plan_cache.sql`) and embedded for similarity search. The cache is keyed by the goal embedding, so semantically equivalent goals (e.g., "deploy staging" and "deploy the staging environment") can share the same template.

## Subgoal-Aware Compaction

When task orchestration is active, the context compaction system tracks subgoal boundaries within the conversation. The `SubgoalRegistry` records which message ranges belong to each subgoal and their completion state (Active, Completed, Abandoned).

During hard compaction, the summarizer preserves messages associated with active subgoals while aggressively compacting completed subgoal ranges. This prevents compaction from destroying the context that an in-progress orchestration task depends on.

## Limitations

- **English-only keyword routing:** The `RuleBasedRouter` step 2 (tool keyword matching) only recognizes English keywords such as "implement", "build", "edit". Task descriptions in other languages always fall through to the first-available-agent fallback. Use explicit `agent_hint` values in planner output for reliable routing.
- **Task count cap:** The `max_tasks` limit (default 20) is enforced at planning time. Graphs exceeding this limit are rejected by `dag::validate` and must be decomposed into smaller sub-goals.
- **No dynamic re-planning:** Once a `TaskGraph` is created and confirmed, its structure is fixed. Tasks cannot be added or removed during execution; only their status and results change.
- **No hot-reload of orchestration config:** Changes to the `[orchestration]` section of `config.toml` require a restart to take effect.
- **`planner_model` and `planner_max_tokens` are reserved:** These config fields are parsed and stored but not yet applied at runtime. `LlmPlanner` uses whatever provider it receives at construction time regardless of `planner_model`.
- **Residual prompt injection risk:** Task descriptions and cross-task context are wrapped in `ContentSanitizer` spotlight tags to mitigate prompt injection, but the risk is not fully eliminated — treat orchestrated task outputs with appropriate caution.
- **Single-agent inline execution:** When no sub-agents are defined, tasks run inline on the main provider in sequence (no parallelism). Configure `[agents]` entries and `max_parallel > 1` for concurrent execution.

## Related

- [Sub-Agent Orchestration](../advanced/sub-agents.md) — sub-agents that execute individual tasks
- [Feature Flags](../reference/feature-flags.md)
- [Configuration](../reference/configuration.md) — full config reference
