---
aliases:
  - Background Task Supervisor
  - Task Supervisor
  - Background Work Management
tags:
  - sdd
  - spec
  - concurrency
  - background-work
created: 2026-04-11
status: approved
related:
  - "[[MOC-specs]]"
  - "[[002-agent-loop/spec]]"
  - "[[036-prometheus-metrics/spec]]"
  - "[[001-system-invariants/spec#5. Concurrency Contract]]"
  - "[[043-zeph-common/spec]]"
---

# Spec: Supervised Background Task Manager

> [!info]
> **v0.19.1**: `TaskSupervisor` is the unified lifecycle manager for all supervised background
> work. Phase 2 enhancements are complete. Leaf crates (`TelegramChannel`, `A2aServer`,
> background indexer) are migrated. Bootstrap memory loops use `TaskSupervisor`. CPU/wall-time
> metrics, blocking semaphore, `BlockingSpawner` trait, and TUI task registry are shipped.

**Issues**: #2816 (Phase 1), #2958 #2960 #2961 #2963 #2978 (Phase 2 / v0.19.1)  
**Status**: Complete  
**Crate**: `zeph-core::agent::supervisor`, `zeph-common` (BlockingSpawner trait)  
**Dependencies**: spec [[002-agent-loop/spec]], [[036-prometheus-metrics/spec]], [[043-zeph-common/spec]]

---

## 1. Problem Statement

The agent loop spawns background work in multiple places with `tokio::spawn()`:

```rust
// Embedding backfill (memory)
tokio::spawn(async move { backfill_embeddings().await; });

// Skill self-learning
tokio::spawn(async move { learn_from_feedback().await; });

// Metric export
tokio::spawn(async move { export_metrics().await; });

// Hook execution
tokio::spawn(async move { fire_hooks().await; });
```

### 1.1 Current Issues

| Issue | Impact | Example |
|-------|--------|---------|
| **No supervision** | Spawned tasks are dropped and untrackable | Backfill task crashes silently; no log, no metrics |
| **Unbounded spawns** | Can exceed OS limits or exhaust memory | Embedding backfill spawns per message; loop spawns 1000s of tasks without bound |
| **No lifecycle control** | Tasks persist after turn boundary | A slow embedding task from turn 1 is still running at turn 10 |
| **Weak observability** | No count of inflight tasks, dropped work, or completion rate | TUI shows no background work; `bg_inflight` metric unimplemented |
| **Contention with loop** | Background tasks compete for LLM API calls and shared state | Embedding backfill blocks memory access, slowing compaction |
| **Orphaned tasks on shutdown** | Tasks continue after agent exits | Agent closes; embedding backfill task still running, consuming resources |

### 1.2 Business Impact

- **Degraded UX**: Agent feels sluggish because background work starves foreground tasks
- **Incorrect metrics**: Prometheus gaps for `bg_inflight`, `bg_dropped` hamper observability
- **Cost overrun**: Orphaned embeddings continue calling the provider after the agent exits
- **Hard-to-debug bugs**: Silent task failures leave no trace in logs

---

## 2. Solution: BackgroundSupervisor

A lightweight supervisor that:

1. Tracks all background tasks per agent session via `tokio::task::JoinSet<TaskResult>`
2. Enforces per-class concurrency limits (not queue depth; see Phase 2E)
3. Drops work immediately when class limit is reached (drop-on-overflow policy)
4. Exports per-class metrics for observability
5. Cleans up all tasks at agent shutdown with `abort_all()`

### 2.1 Implementation (Complete)

**Struct**: `TaskSupervisor` at `crates/zeph-core/src/agent/supervisor.rs`

**Key design**:
- Shared via `Arc<TaskSupervisor>` — usable by leaf crates without `zeph-core` ownership
- Per-class inflight counter using `Arc<AtomicUsize>` (read-only from spawned tasks)
- RAII `InflightGuard` decrements inflight immediately on task completion
- `reap()` called non-blocking at turn boundary to drain completed tasks
- `shutdown_all(timeout)` called on agent shutdown — drains completions before exiting, then aborts remaining tasks
- Task names are `Arc<str>` — no `Box::leak` or `&'static str` required
- `spawn_blocking` routes through a `tokio::sync::Semaphore` (default capacity 8) to bound OS thread pool usage
- `spawn_restartable` supports `RestartPolicy::Restart { max, base_delay }` with exponential backoff
- All tasks visible in `list_tasks()` / `TaskSnapshot` for TUI and observability

**Location in agent loop**:
```
persistence.rs:
  - spawn_summarization()        — summarization signal
  - spawn() for enrichment tasks — graph/persona/trajectory/audit
corrections.rs:
  - spawn() for audit tasks
mod.rs:
  - reap() at turn boundary
  - supervisor.shutdown_all(10s) on orderly shutdown
runner.rs:
  - 7 memory background loops registered via supervisor (RestartPolicy::RunOnce)
  - Single shutdown_rx → CancellationToken bridge (replaces 7 per-loop bridges)
daemon.rs:
  - A2aServer registered via supervisor (RestartPolicy::RunOnce)
agent_setup.rs:
  - Background indexer routed through supervisor when present
zeph-channels:
  - TelegramChannel dispatcher via supervisor (RestartPolicy::Restart { max: 5, base_delay: 2s })
```

### 2.2 Architecture

```
BackgroundSupervisor (owned by LifecycleState)
├── JoinSet<TaskResult> (all inflight tasks)
├── class_inflight: [Arc<AtomicUsize>; 2]
│   └── [0] Enrichment (limit 4)
│   └── [1] Telemetry (limit 8)
├── class_metrics: [ClassMetrics; 2]
│   ├── spawned: u64
│   ├── dropped: u64
│   └── completed: u64
└── SummarizationSignal (communicates summarization completion to foreground)
```

---

## 3. Task Priority Classes (Phase 1)

Only two classes are implemented in Phase 1.

| Class | Limit | Policy | Examples |
|-------|-------|--------|----------|
| **Enrichment** | 4 concurrent | Drop on limit | Summarization, graph/persona/trajectory extraction, audit logs |
| **Telemetry** | 8 concurrent | Drop on limit | Metrics export, graph count sync |

### 3.1 Rationale

- **Enrichment**: "Nice-to-have" background work that enriches memory and observability. Can be safely dropped when limit is exceeded. Lossy by design.
- **Telemetry**: Faster background metrics/state updates. Larger limit because these are cheap operations.
- **No Critical class**: Critical work (LLM calls, tool execution, message persistence) runs on the foreground `await` path in `persist_message()` and `execute_tool_calls_batch()`. It is never spawned as background work — concurrency control for critical operations is the responsibility of the foreground loop and timeout guards. Background tasks are explicitly lossy.

---

## 4. Concurrency Limits (Phase 1)

### 4.1 Current Implementation

Limits are **hardcoded** per class (not configurable in Phase 1):

```rust
impl TaskClass {
    fn max_concurrency(self) -> usize {
        match self {
            TaskClass::Enrichment => 4,
            TaskClass::Telemetry => 8,
        }
    }
}
```

When a spawn request arrives for a class at its limit, the task is dropped immediately (not queued):

```rust
let current = self.class_inflight[idx].load(Ordering::Relaxed);
if current >= class.max_concurrency() {
    // Task is dropped, bg_dropped counter incremented
    self.class_metrics[idx].dropped += 1;
    return false;
}
```

### 4.2 Phase 2E: Configurable Depths (#2888)

Future enhancement to move limits to config:

```toml
[agent.supervisor]
enrichment_limit = 4
telemetry_limit = 8
```

### 4.3 Turn-Boundary Cleanup

`abort_class(TaskClass::Enrichment)` is available and called at turn boundary (if configured via `abort_enrichment_on_turn`). See `TaskSupervisorConfig` in Section 5.1.

---

## 5. Integration

### 5.1 API

```rust
pub struct TaskSupervisor {
    // internal JoinSet + per-class state + blocking semaphore
}

impl TaskSupervisor {
    pub fn new(config: &TaskSupervisorConfig) -> Arc<Self>;

    /// Spawn an async background task under `class`.
    /// Returns true when accepted, false when dropped due to class limit.
    pub fn spawn(
        &self,
        class: TaskClass,
        name: Arc<str>,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> bool;

    /// Spawn a restartable task with the given restart policy.
    pub fn spawn_restartable(
        &self,
        name: Arc<str>,
        policy: RestartPolicy,
        fut: impl Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    );

    /// Spawn a CPU-bound blocking task via OS thread pool (gated by semaphore).
    pub fn spawn_blocking(
        &self,
        name: Arc<str>,
        f: impl FnOnce() + Send + 'static,
    );

    /// Variant for summarization tasks that signal completion via SummarizationSignal.
    pub fn spawn_summarization(
        &self,
        name: Arc<str>,
        fut: impl Future<Output = bool> + Send + 'static,
    ) -> bool;

    /// Poll all completed tasks without blocking.
    pub fn reap(&self) -> SummarizationSignal;

    /// Gracefully drain completions, then abort remaining tasks after `timeout`.
    pub async fn shutdown_all(&self, timeout: Duration);

    /// Abort all tasks in a class immediately.
    pub fn abort_class(&self, class: TaskClass);

    /// Snapshot all registered tasks (name, state, uptime, restart count).
    pub fn list_tasks(&self) -> Vec<TaskSnapshot>;

    /// Per-class metrics snapshot.
    pub fn metrics_snapshot(&self) -> SupervisorMetrics;
}

/// Config struct (nested in AgentConfig as `agent.supervisor`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskSupervisorConfig {
    pub enrichment_limit: usize,          // default: 4
    pub telemetry_limit: usize,           // default: 8
    pub blocking_semaphore_capacity: u32, // default: 8
    pub abort_enrichment_on_turn: bool,   // default: false
}
```

### 5.2 BlockingSpawner Trait

`BlockingSpawner` is defined in `zeph-common` to break the `zeph-core → zeph-index` crate cycle:

```rust
// zeph-common::blocking_spawner
pub trait BlockingSpawner: Send + Sync + 'static {
    fn spawn_blocking_named(&self, name: Arc<str>, f: Box<dyn FnOnce() + Send + 'static>);
}
```

`TaskSupervisor` in `zeph-core` implements `BlockingSpawner` by delegating to its `spawn_blocking` method. `CodeIndexer` in `zeph-index` accepts `Option<Arc<dyn BlockingSpawner>>` via a `with_spawner()` builder. Each `chunk_file` invocation uses a unique task name `chunk_file_{N}` (AtomicU64 counter) so concurrent indexing sessions are fully visible in `list_tasks()`.

See [[043-zeph-common/spec]] for the `BlockingSpawner` trait definition.

### 5.2 Usage Pattern

Spawning from `persist_message()`:

```rust
// persistence.rs:602
self.lifecycle.supervisor.spawn_summarization("summarization", async move {
    let did_summarize = maybe_summarize_older_pairs(...).await?;
    Ok(did_summarize)
});

// persistence.rs:734 — graph extraction
self.lifecycle.supervisor.spawn(
    TaskClass::Enrichment,
    "graph-extract",
    async move {
        extract_and_sync_graph(...).await.ok();
    },
);
```

Reaping at turn boundary (`mod.rs:1047`):

```rust
let bg_signal = self.lifecycle.supervisor.reap();
if bg_signal.did_summarize {
    self.lifecycle.state.msg.unsummarized_count = 0;
}
```

Cleanup at shutdown (`mod.rs:646`):

```rust
self.lifecycle.supervisor.abort_all();
```

---

## 6. Observability & Metrics (Phase 1 & Phase 2)

### 6.1 Metrics (Implemented)

The supervisor tracks per-class counters in `ClassMetrics`:

| Metric | Type | Exported | Unit | Notes |
|--------|------|----------|------|-------|
| `spawned` | Counter | To Prometheus as `bg_spawned` | tasks | Total tasks spawned since agent start |
| `dropped` | Counter | To Prometheus as `bg_dropped` | tasks | Total tasks dropped due to class limit |
| `completed` | Counter | To Prometheus as `bg_completed` | tasks | Total tasks completed (success or panic) |
| `inflight` | Gauge | To Prometheus as `bg_inflight` | tasks | Current count of tasks in flight (not queued — actually running) |

Metrics are snapshottable via `metrics_snapshot()` for logging and TUI display.

### 6.2 CPU/Wall-Time Metrics (task-metrics feature)

When the `task-metrics` feature is enabled (included in `full`), `spawn_blocking` wraps each task with `cpu-time::ThreadTime` + `Instant` measurements and emits:

- `metrics::histogram!("zeph.task.cpu_time_ms")` — CPU time per blocking task
- `metrics::histogram!("zeph.task.wall_time_ms")` — wall time per blocking task

Both histogram values are also recorded as tracing span fields `task.cpu_time_ms` and `task.wall_time_ms`, visible in Jaeger and tokio-console.

Zero overhead when the feature is disabled (`#[inline]` identity fn, no deps linked).

### 6.3 TUI Task Registry Panel

See [[011-tui/spec]] for `TaskRegistryWidget` and `/tasks` command. The widget calls `supervisor.list_tasks()` each render cycle to populate its table.

---

## 7. Task Naming (Phase 1)

Each spawned task is tagged with a `name: &'static str` parameter for logging and diagnostics:

```rust
supervisor.spawn(
    TaskClass::Enrichment,
    "graph-extract",    // task name for logs
    async move { ... },
);

supervisor.spawn_summarization(
    "summarization",    // task name for logs
    async { true },
);
```

Task names appear in debug-level logs:

```
TRACE: background task spawned class=enrichment task=graph-extract
TRACE: background task dropped class=enrichment task=graph-extract limit=4
```

**Phase 2 Note**: Task tagging and coalescing (skipping duplicate kinds) are **not implemented** in Phase 1. All tasks are spawned if slot is available.

---

## 8. Key Invariants

### Always
- All supervised tasks are tracked in the internal `JoinSet` + task registry (never orphaned)
- Per-class inflight counters are updated atomically via `Arc<AtomicUsize>`
- When a task completes, its inflight slot is freed immediately (via `InflightGuard` drop)
- `reap()` is non-blocking and idempotent — safe to call every turn
- `shutdown_all(timeout)` uses a **two-phase drain**: phase 1 runs the normal completion loop; phase 2 continues draining after the cancellation token fires until the completion channel is empty. This ensures tasks that observe the cancellation token slightly after it is set still have their results collected and are not silently discarded. Only after both phases are exhausted (or the timeout fires) are remaining tasks aborted.
- Task names are `Arc<str>` — never `&'static str` or `Box::leak`
- `spawn_blocking` is gated by the blocking semaphore (default capacity 8)
- `spawn_restartable` uses exponential backoff for `RestartPolicy::Restart`
- `list_tasks()` includes blocking tasks and oneshot tasks — fully observable
- Metrics snapshots are consistent (spawned ≥ dropped + completed at any point in time)

### Ask First
- Adding new background task spawn sites outside the supervisor
- Changing per-class concurrency limits without testing load behavior
- Increasing blocking semaphore capacity above 8

### Never
- Spawn a task with `tokio::spawn()` directly — always use `supervisor.spawn()`, `spawn_restartable()`, `spawn_blocking()`, or `spawn_summarization()`
- Hold a lock across `supervisor.spawn()` call (deadlock risk with the inflight atomic)
- Allow background tasks to panic — all spawned futures must use proper error handling
- Assume a background task completed successfully without checking metrics

---

## 9. Example: Embedding Backfill

Current code (problematic):

```rust
// In memory compaction logic
tokio::spawn(async move {
    match backfill_embeddings(&messages).await {
        Ok(_) => {}, // silently succeeds
        Err(e) => {}, // silently fails, no log
    }
});
```

With supervisor:

```rust
// In memory compaction logic
supervisor.spawn(
    TaskClass::Enrichment,
    async {
        match backfill_embeddings(&messages).await {
            Ok(_) => {
                tracing::debug!("embedding backfill completed");
                Ok(())
            }
            Err(e) => {
                tracing::warn!("embedding backfill failed: {e}");
                Err(TaskError::Backfill(e.to_string()))
            }
        }
    }
);
```

Benefits:

- ✅ Task is counted in `bg_inflight` gauge
- ✅ On completion, `bg_completed` is incremented
- ✅ If queue depth is exceeded, new backfill is dropped, `bg_dropped` incremented
- ✅ At turn boundary, backfill is aborted (prevents stale work blocking new turns)
- ✅ TUI shows background activity: `bg: 1 enrich`
- ✅ At shutdown, backfill is cleanly cancelled

---

## 10. Out of Scope

The following systems are **explicitly excluded** from the supervisor's purview:

- **Scheduler cron loops**: The `zeph-scheduler` cron loop has its own `CancellationToken` lifecycle and is not turn-scoped. Do not wrap it in `TaskSupervisor`.
- **Critical path tasks**: LLM calls, tool execution, message persistence. These run on the foreground `await` path with timeout guards and are not background work.
- **Session digest on shutdown**: `spawn_outgoing_digest()` uses a plain `tokio::spawn` at shutdown because it requires access to fields that would create a borrow conflict with `supervisor.spawn()` at the session-close boundary.
- **Task coalescing and deduplication**: Multiple spawns of the same task kind are not coalesced — each spawn gets a unique entry in the registry.
- **Turn-boundary abort**: Automatic cleanup of enrichment tasks at turn boundaries is Phase 2D (#2887).

---

## 11. Open Questions (Resolved)

1. **Turn-boundary abort location** — RESOLVED
   - Chosen: Option A (explicit call in agent loop)
   - Status: Not implemented in Phase 1; see Phase 2D (#2887)

2. **Critical class limits** — RESOLVED
   - Decision: Critical class not needed — all critical work is foreground
   - Rationale: LLM calls, tool execution, message persistence run on the main `await` path with timeout guards. Background tasks are inherently lossy. No background task is critical.

3. **Task error handling** — RESOLVED
   - Policy: Log at debug level on completion, warn if task panics
   - No retry — transient failures are acceptable for lossy background work
   - Implementation: Panics logged via `Err` branch in `reap()` at warn level

---

## 12. Success Criteria

- [x] `TaskSupervisor` struct compiles and integrates with agent loop (PR #2816)
- [x] Enrichment task sites use the supervisor: summarization, graph/persona/trajectory extraction, audit logging
- [x] Per-class inflight limits enforced and configurable via `[agent.supervisor]` config
- [x] Metrics (`spawned`, `dropped`, `completed`, `inflight`) exported per class
- [x] `shutdown_all(10s)` drains completions before aborting remaining tasks
- [x] `spawn_restartable` with `RestartPolicy::Restart` applies exponential backoff
- [x] `TaskStatus::Aborted` state for force-aborted entries; `list_tasks()` reflects all states
- [x] 6 regression tests for shutdown/restart/blocking semantics (PR #2958)
- [x] 7 bootstrap memory loops migrated to supervisor (PR #2960)
- [x] `TelegramChannel`, `A2aServer`, background indexer migrated (PR #2961)
- [x] TUI `/tasks` panel shows live task registry (PR #2962)
- [x] CPU/wall-time metrics via `task-metrics` feature (PR #2963)
- [x] `BlockingSpawner` trait in `zeph-common` breaks `zeph-core → zeph-index` cycle (PR #2978)
- [x] Blocking semaphore (capacity 8) prevents OS thread pool saturation (PR #3009)
- [x] Task names are `Arc<str>` — no `Box::leak` per indexed file (PR #3005)

---

## See Also

- [[MOC-specs]] — all specifications
- [[002-agent-loop/spec]] — agent loop structure and turn lifecycle
- [[036-prometheus-metrics/spec]] — metrics export and schema
- [[001-system-invariants/spec#5. Concurrency Contract]] — concurrency invariants
- GitHub PR #2816 — Phase 1 implementation
- GitHub Epic #2883 — Phase 2 coordination
