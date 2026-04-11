---
aliases:
  - Background Task Supervisor
  - Task Supervisor
  - Background Work Management
tags:
  - sdd
  - spec
  - draft
  - concurrency
  - background-work
  - proposed
created: 2026-04-11
status: draft
related:
  - "[[MOC-specs]]"
  - "[[002-agent-loop/spec]]"
  - "[[036-prometheus-metrics/spec]]"
  - "[[001-system-invariants/spec#5. Concurrency Contract]]"
---

# Spec: Supervised Background Task Manager (Proposed)

> [!warning]
> This specification describes a **proposed feature** (not yet implemented).
> It addresses GitHub issue #2816: fire-and-forget tokio::spawn calls throughout the
> agent loop lack unified supervision, observability, and lifecycle control.

**Issue**: #2816  
**Status**: Draft (design phase)  
**Crate**: `zeph-core` (proposed module)  
**Dependencies**: spec [[002-agent-loop/spec]], [[036-prometheus-metrics/spec]]

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

## 2. Proposed Solution: AgentTaskSupervisor

A lightweight supervisor that:

1. Tracks all background tasks per agent session
2. Enforces per-class queue depth limits
3. Drops or coalesces work when limits are exceeded
4. Aborts Enrichment tasks at turn boundaries to prevent backlog
5. Exports metrics for observability
6. Cleans up all tasks at agent shutdown

### 2.1 Architecture

```
AgentTaskSupervisor (one per agent session)
├── JoinSet (all inflight tasks)
├── TaskQueue
│   ├── Critical (turn loop, provider dispatch)
│   ├── Enrichment (embeddings, skill learn, graph extract)
│   └── Telemetry (metrics sync, profiling)
└── Metrics
    ├── bg_inflight (gauge)
    ├── bg_dropped (counter)
    └── bg_completed (counter)
```

---

## 3. Task Priority Classes

Each spawned task belongs to one of three priority classes:

| Class | Priority | Max Queue | Policy | Examples |
|-------|----------|-----------|--------|----------|
| **Critical** | Highest | — | Never drop; any blocking is fatal | Agent loop continuation, LLM provider calls, tool execution, message persistence |
| **Enrichment** | Medium | 5–10 | Drop on overflow; abort at turn boundary | Embedding backfill, skill self-learning, graph extraction, ARISE trace improvement, compaction probe |
| **Telemetry** | Lowest | 2–3 | Drop on overflow; coalesce (skip if pending) | Metric sync, profiling snapshot, audit trail flush |

### 3.1 Rationale

- **Critical**: Must complete; failures block the agent → no limit
- **Enrichment**: Nice-to-have; can be safely dropped or restarted → bounded queue
- **Telemetry**: Observability only; very cheap to recompute → smallest queue

---

## 4. Queue Depth Limits and Drop Policy

### 4.1 Configuration

```toml
[agent.task_supervisor]
# Maximum tasks in Enrichment queue (0 = disabled)
enrichment_queue_depth = 8

# Maximum tasks in Telemetry queue
telemetry_queue_depth = 2

# Abort Enrichment tasks at turn boundary?
abort_enrichment_on_turn_boundary = true

# Drop mode: "drop_oldest" | "drop_new" | "coalesce"
drop_policy = "drop_oldest"
```

### 4.2 Drop Behavior

When `enrich_queue_depth` is reached and a new Enrichment task is requested:

- **drop_oldest**: Abort the oldest queued task, spawn the new one
- **drop_new**: Skip the new task (return early, increment `bg_dropped`)
- **coalesce**: If a similar task is already queued, skip this one

### 4.3 Turn-Boundary Cleanup

At the end of each turn, if `abort_enrichment_on_turn_boundary = true`:

```rust
// In agent loop at end of turn
supervisor.abort_class(TaskClass::Enrichment);
```

This prevents backlog buildup across turns:

```
Turn 1: spawn embedding backfill (5 turns)
        spawn skill learn (2 turns)
Turn 2: User input arrives
        abort enrichment tasks
        proceed with turn 2 (now inflight enrichment is gone)
```

---

## 5. Integration with Agent Loop

### 5.1 Spawning a Task

In `zeph-core`'s agent loop:

```rust
// Instead of: tokio::spawn(async { ... });
//
// Use:
supervisor.spawn(
    TaskClass::Enrichment,
    async {
        backfill_embeddings().await
            .map_err(|e| tracing::error!("embedding backfill failed: {e}"))
    }
);
```

### 5.2 Constructor

```rust
pub struct AgentTaskSupervisor {
    tasks: JoinSet<Result<(), TaskError>>,
    config: TaskSupervisorConfig,
    metrics: TaskMetrics,
}

impl AgentTaskSupervisor {
    pub fn new(config: TaskSupervisorConfig) -> Self { ... }
    
    pub fn spawn(&mut self, class: TaskClass, future: impl Future + Send + 'static) {
        if self.should_accept(class) {
            self.tasks.spawn(future);
            self.metrics.bg_inflight.inc();
        } else {
            self.metrics.bg_dropped.inc();
        }
    }
    
    pub fn abort_class(&mut self, class: TaskClass) {
        // Abort all tasks tagged with this class
    }
    
    pub async fn cleanup(&mut self) {
        // Abort all remaining tasks and wait for cancellation
    }
}
```

### 5.3 Turn-Boundary Cleanup Hook

In `run_agent_loop()`:

```rust
loop {
    // ... normal turn logic ...
    
    // At end of turn
    if should_cleanup_background_work() {
        supervisor.abort_class(TaskClass::Enrichment);
    }
    
    // Collect completed task results (non-blocking)
    supervisor.poll_completed();
}
```

---

## 6. Observability & Metrics

The supervisor exports metrics compatible with spec [[036-prometheus-metrics/spec]]:

| Metric | Type | Labels | Unit | Notes |
|--------|------|--------|------|-------|
| `bg_inflight` | Gauge | `class: enrichment \| telemetry \| critical` | tasks | Current count of background tasks in flight |
| `bg_dropped` | Counter | `class, reason: overflow \| coalesce` | tasks | Cumulative count of dropped tasks |
| `bg_completed` | Counter | `class, status: ok \| error` | tasks | Cumulative count of completed tasks |
| `bg_latency` | Histogram | `class` | ms | Task completion time (p50, p95, p99) |

### 6.1 TUI Integration

The TUI status bar shows:

```
bg: 2 enrich, 0 telem | ↻ backfill...
```

Meaning: 2 enrichment tasks inflight, 0 telemetry, with the current operation being backfill.

---

## 7. Task Tagging & Coalescing

To enable smart coalescing, tasks are tagged with a `TaskKind`:

```rust
pub enum TaskKind {
    EmbeddingBackfill,
    SkillSelfLearn,
    GraphExtract,
    MetricsSync,
    ProfilingSnapshot,
    // ...
}

pub struct SupervisorTask {
    kind: TaskKind,
    class: TaskClass,
    spawned_at: Instant,
}
```

When coalescing is enabled and a task of `kind = EmbeddingBackfill` is already queued,
a new `EmbeddingBackfill` request returns immediately (no spawn, no metric increment).

---

## 8. Key Invariants

### Always
- Critical class tasks are never dropped or aborted
- Enrichment class tasks can be dropped at turn boundaries without data loss
- Telemetry class tasks are always coalesced (never execute the same kind twice simultaneously)
- All spawned tasks are tracked in a `JoinSet` (not dropped, not orphaned)
- Metrics are incremented/decremented atomically with task lifecycle events
- At agent shutdown, all background tasks are aborted and waited on (graceful cleanup)

### Ask First
- Setting `abort_enrichment_on_turn_boundary = false` (risks backlog buildup)
- Using `drop_policy = "drop_new"` (may miss important updates)
- Spawning Critical class tasks from user tool code (only agent loop should spawn Critical)

### Never
- Spawn a task and drop the `JoinHandle` without tracking it in the supervisor
- Hold a lock across a background task spawn (deadlock risk)
- Spawn unbounded task counts (always enforce queue limits per class)
- Allow background tasks to panic (all spawned tasks must wrap Err → log, not panic)

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

## 10. Phase 2: Dynamic Priority Adjustment

Future enhancement (not Phase 1):

- **Adaptive queue depth**: Increase `enrichment_queue_depth` when the agent is idle, decrease when busy
- **Priority escalation**: Promote Enrichment tasks to Critical after N turns of queueing (don't drop stale work forever)
- **Cost-aware dropping**: Drop expensive tasks (e.g., expensive embedding backfill) before cheap ones (profiling)

---

## 11. Open Questions

1. **Where should turn-boundary abort happen?**
   - Option A: Explicit call in agent loop: `supervisor.abort_class(TaskClass::Enrichment)`
   - Option B: Automatic cleanup in turn-end hook (requires supervisor to know turn lifecycle)
   - Option C: Config-driven (always abort vs never abort)
   - Recommendation: Option A (explicit, visible in code)

2. **Should Critical class have any limits?**
   - Currently: unbounded (no limit)
   - Option A: Small hard limit (100) to catch runaway spawns
   - Option B: Keep unbounded (trust agent loop to not spam)
   - Recommendation: Option B for Phase 1, Option A for Phase 2 safety

3. **How should task errors be handled?**
   - Should a failed Enrichment task retry, or just log?
   - Should failed Telemetry cause a warning in logs?
   - Recommendation: Log at debug level; no retry (transient failures are OK to skip)

---

## 12. Success Criteria

Phase 1 acceptance criteria:

- [ ] `AgentTaskSupervisor` struct compiles and integrates with agent loop
- [ ] At least one Enrichment task (embedding backfill) uses the supervisor
- [ ] Queue depth limits are enforced and tested
- [ ] Metrics (`bg_inflight`, `bg_dropped`) are exported to Prometheus
- [ ] TUI status bar shows background task count
- [ ] All background tasks are cleaned up on agent shutdown (no panics in Drop)
- [ ] Integration tests pass with supervisor enabled and disabled (via config)

---

## See Also

- [[MOC-specs]] — all specifications
- [[002-agent-loop/spec]] — agent loop structure and turn lifecycle
- [[036-prometheus-metrics/spec]] — metrics export and schema
- [[001-system-invariants/spec#5. Concurrency Contract]] — concurrency invariants
- GitHub issue #2816 — original issue requesting supervised background work
