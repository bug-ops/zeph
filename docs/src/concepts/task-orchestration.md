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
```

## Related

- [Sub-Agent Orchestration](../advanced/sub-agents.md) — sub-agents that execute individual tasks
- [Feature Flags](../reference/feature-flags.md) — enabling the `orchestration` feature
- [Configuration](../reference/configuration.md) — full config reference
