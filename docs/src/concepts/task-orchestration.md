# Task Orchestration

Task orchestration decomposes complex goals into a directed acyclic graph (DAG) of dependent tasks, executes them in parallel where possible, and handles failures with configurable strategies. It is an optional, feature-gated component (`--features orchestration`) that persists graph state in SQLite so execution survives restarts.

## Prerequisites

Enable the `orchestration` feature flag before building:

```bash
cargo build --release --features orchestration
```

See [Feature Flags](../reference/feature-flags.md) for the full flag list.

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
| `Cancel` | Cancel a running sub-agent (on graph abort or skip propagation) |
| `Done` | Graph reached a terminal or paused state |

The scheduler never holds a mutable reference to `SubAgentManager` — it produces actions for the caller to execute (command pattern). This keeps the scheduler testable in isolation and avoids borrow conflicts.

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

### SubAgentManager Integration

`SubAgentManager::spawn_for_task()` wraps the standard `spawn()` method and hooks into the scheduler's event channel. When the sub-agent's `JoinHandle` resolves, it automatically sends a `TaskEvent` to the scheduler. This is minimally invasive — no changes to `SubAgentHandle` or `run_agent_loop` internals.

## CLI Commands

| Command | Description |
|---------|-------------|
| `/plan <goal>` | Decompose goal into a DAG and execute |
| `/plan status` | Show current graph progress |
| `/plan cancel` | Cancel the active graph |

## Configuration

Add an `[orchestration]` section to `config.toml`:

```toml
[orchestration]
enabled = true
max_tasks = 20                      # Maximum tasks per graph (default: 20)
max_parallel = 4                    # Maximum concurrent task executions (default: 4)
default_failure_strategy = "abort"  # abort, retry, skip, or ask (default: "abort")
default_max_retries = 3             # Retries for the "retry" strategy (default: 3)
task_timeout_secs = 300             # Per-task timeout in seconds, 0 = no timeout (default: 300)
# planner_model = "claude-sonnet-4-20250514"  # Model override for planning LLM calls
planner_max_tokens = 4096           # Max tokens for planner response (default: 4096; reserved)
dependency_context_budget = 16384   # Character budget for cross-task context (default: 16384)
confirm_before_execute = true       # Show confirmation before executing a plan (default: true)
```

## Related

- [Sub-Agent Orchestration](../advanced/sub-agents.md) — sub-agents that execute individual tasks
- [Feature Flags](../reference/feature-flags.md) — enabling the `orchestration` feature
- [Configuration](../reference/configuration.md) — full config reference
