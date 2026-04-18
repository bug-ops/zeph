---
aliases:
  - Orchestration
  - DAG Planning
  - Task Scheduling
tags:
  - sdd
  - spec
  - orchestration
  - planning
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[002-agent-loop/spec]]"
  - "[[005-skills/spec]]"
  - "[[023-complexity-triage-routing/spec]]"
---

# Spec: Orchestration

> [!info]
> DAG planner, DagScheduler, AgentRouter, /plan command, plan template cache,
> adaptive replanning, cascade-aware DAG routing, tree-optimized dispatch.

## Sources

### External
- **LLMCompiler** (ICML 2024) — parallel tool call dispatch, 3.7× latency improvement: https://arxiv.org/abs/2312.04511
- **RouteLLM** (ICML 2024) — cost-quality routing, Thompson Sampling background: https://arxiv.org/abs/2406.18665
- **Unified LLM Routing + Cascading** (ICLR 2025) — escalate on quality threshold: https://openreview.net/forum?id=AAl89VNNy1

### Internal
| File | Contents |
|---|---|
| `crates/zeph-core/src/orchestration/mod.rs` | `OrchestrationEngine`, public API |
| `crates/zeph-core/src/orchestration/dag.rs` | `TaskGraph`, DAG structure (petgraph) |
| `crates/zeph-core/src/orchestration/scheduler.rs` | `DagScheduler`, tick loop |
| `crates/zeph-core/src/orchestration/planner.rs` | `LlmPlanner`, goal decomposition |
| `crates/zeph-core/src/orchestration/router.rs` | `AgentRouter`, 3-step fallback |
| `crates/zeph-core/src/orchestration/aggregator.rs` | `LlmAggregator`, per-task token budget |
| `crates/zeph-core/src/orchestration/command.rs` | `/plan` command parsing |
| `crates/zeph-core/src/orchestration/graph.rs` | Internal graph utilities |
| `crates/zeph-core/src/orchestration/error.rs` | `OrchestrationError` |

---

`crates/zeph-core/src/orchestration/` (feature: `orchestration`) — DAG task planning and execution.

## Components

```
OrchestrationEngine
├── LlmPlanner        — goal → TaskGraph (structured output from LLM)
├── TaskGraph         — DAG of tasks with dependencies (petgraph)
├── DagScheduler      — tick-based executor, respects dependency edges
├── AgentRouter       — routes tasks to sub-agents (rule-based, 3-step fallback)
└── LlmAggregator     — merges sub-agent results, per-task token budget
```

## Planning Flow

1. User provides goal (via `/plan goal <text>` or natural language)
2. `LlmPlanner` decomposes goal into `Task` nodes via structured output (JSON schema)
3. `TaskGraph` built as directed acyclic graph — edges represent dependencies
4. `/plan confirm` required before execution begins (user approval gate)
5. `DagScheduler` ticks: ready tasks (all deps resolved) are dispatched in parallel
6. Results flow through `LlmAggregator` which merges with per-task token budget

## AgentRouter (3-Step Fallback)

1. Exact rule match: config-defined `router_rules` (task type → agent name)
2. Capability match: check registered sub-agents for capability overlap
3. Default: route to primary agent

## Task States

```
Pending → Queued → Running → Completed
                 → Failed → Retryable (max 3 retries)
                          → Aborted
```

- `/plan cancel <id>` transitions Running → Aborted
- `/plan retry <id>` transitions Failed → Pending

## `/plan` CLI Commands

| Command | Action |
|---|---|
| `/plan goal <text>` | Decompose goal into DAG |
| `/plan status` | Show current plan status |
| `/plan list` | List all tasks with states |
| `/plan confirm` | Approve and start execution |
| `/plan cancel [id]` | Cancel task or entire plan |
| `/plan resume` | Resume paused plan |
| `/plan retry <id>` | Retry failed task |

## TUI Integration

- `PlanView` widget toggled with `p` key
- Shows DAG visualization, task states, progress
- Running tasks show spinner (mandatory per TUI rules)

## LlmPlanner Multi-Model Design

`LlmPlanner` accepts any `LlmProvider` — the caller selects the provider at construction time based on `OrchestrationConfig::planner_provider`.

### Config

```toml
[orchestration]
planner_provider = "quality"   # references [[llm.providers]] name; empty = primary provider fallback
```

- `planner_provider: String` — provider name from `[[llm.providers]]`. Empty string means "use the agent's primary provider".
- `planner_model` has been removed (dead field, cleaned up pre-v1.0.0). Config migration `migrate_planner_model_to_provider()` rewrites any existing `planner_model` key with a warning to use `planner_provider` instead.

### Provider selection rule

Planning is a complex/expert task (goal decomposition requires reasoning about parallelism and dependencies) — route to a quality provider, not a fast/cheap one.

```
planner_provider = "quality"  # correct: complex reasoning task
planner_provider = "fast"     # acceptable only for simple, known-structure goals
```

### Key Invariants

- User confirmation (`/plan confirm`) is required before any task execution — never auto-start
- `LlmAggregator` must enforce per-task token budget — runaway tasks must be truncated
- `TaskGraph` must be a true DAG — cycles are a hard error, not a warning
- `DagScheduler` is tick-based (not event-driven) — tick interval is configurable
- Sub-agent results are merged by `LlmAggregator`, not concatenated — aggregation is an LLM call
- `planner_provider` must resolve via the provider registry at runtime — never hardcode a model in `LlmPlanner`

---

## Plan Template Caching

`crates/zeph-orchestration/src/plan_cache.rs`. Issue #1856.

### Overview

`PlanCache` stores completed `TaskGraph` plans as reusable `PlanTemplate` skeletons in SQLite. On subsequent semantically similar goals, the cache returns the closest template and uses a lightweight LLM adaptation call instead of full goal decomposition, reducing planner cost.

### `PlanTemplate` Structure

Stripped of all runtime state (status, results, retry_count, assigned_agent, timestamps):

```
PlanTemplate {
    goal: String,              // normalized goal text (trim + collapse whitespace + lowercase)
    tasks: Vec<TemplateTask>,  // structural skeleton
}

TemplateTask {
    title, description, agent_hint, depends_on, failure_strategy, task_id
}
```

`task_id`: stable kebab-case slug generated from title + position for `depends_on` reconstruction.

### Cache Lookup

1. Normalize goal: trim + collapse whitespace + lowercase
2. BLAKE3 hash of normalized goal → dedup key for `INSERT OR REPLACE ON CONFLICT(goal_hash)`
3. Cosine similarity computed in-process (no Qdrant) between query embedding and stored template embeddings
4. Return closest template if `similarity >= similarity_threshold` (default 0.90)
5. Lightweight LLM adaptation call: adapts template to the specific goal without full decomposition
6. Any cache failure → graceful degradation to full `planner.plan()` — cache never blocks planning

### Eviction

Two-phase eviction:
1. TTL sweep: delete rows where `created_at < now - ttl_days * 86400`
2. LRU size cap: if `count > max_templates`, delete oldest by `last_used_at`

Stale embeddings: NULLed when embedding model changes (same pattern as `ResponseCache`).

### Config

```toml
[orchestration.plan_cache]
enabled = false           # opt-in
similarity_threshold = 0.90
ttl_days = 30
max_templates = 100
```

### Key Invariants

- Cache failure (DB error, embedding error) always falls back to `planner.plan()` — never surface cache errors to user
- Goal normalization (trim + collapse + lowercase) is mandatory for dedup — never hash un-normalized goal
- Cosine similarity uses in-process math — never depends on Qdrant being available
- `INSERT OR REPLACE ON CONFLICT(goal_hash)` prevents duplicate templates
- Adaptation call is always an LLM call — never return template directly without adaptation
- NEVER block plan execution on cache write — write is best-effort

---

## Inter-Agent Handoff

Inter-agent context propagation uses a skill-based YAML protocol defined in the `rust-agent-handoff` skill. See `specs/handoff-skill-system/spec.md` for the full specification.

There are no typed Rust structs or compile-time validation for handoff content in the orchestration crate. The skill documentation is the contract. Typed validation (PRs #2076, #2078) was attempted and reverted (#2082).

---

## Topology Classification

`TopologyClassifier` — heuristic DAG topology detection. Issues #1840, #2219.

### Topology Variants

| Topology | Description | Default strategy |
|---|---|---|
| `AllParallel` | All tasks independent | `FullParallel` |
| `LinearChain` | All tasks form a sequence | `Sequential` |
| `FanOut` | Single root → many leaves | `Adaptive` |
| `FanIn` | Many sources → single sink | `Adaptive` |
| `Hierarchical` | Multiple levels with partial ordering | `LevelBarrier` |
| `Mixed` | Other | `Adaptive` |

### TopologyAnalysis

`analyze()` returns `TopologyAnalysis { topology, strategy, max_parallel, depth, depths: HashMap<TaskId, usize> }`.

- `classify_with_depths(graph, longest_path, depths)` accepts pre-computed values to avoid redundant toposort
- `compute_max_parallel(topology, base)` is the single canonical source of topology→parallelism policy
- `DagScheduler` stores `config_max_parallel` (immutable) and re-derives `max_parallel` from topology on each analysis — prevents drift across replan cycles

### LevelBarrier Dispatch

For `Hierarchical` topology: tasks are grouped into levels (depth layers). `DagScheduler.tick()` dispatches all tasks at the current level, then waits for all to complete before advancing. `current_level` is reset after `inject_tasks()` inserts a task at depth < current level.

### Config

```toml
[orchestration]
topology_selection = true  # opt-in
```

### Key Invariants

- `compute_max_parallel()` must be called with the immutable `config_max_parallel` as base — never with runtime `self.max_parallel`
- `topology_dirty` flag defers re-analysis to the start of the next `tick()` — never re-analyze mid-tick
- After `self.topology = new_analysis`, `self.max_parallel` must be immediately synced
- `LevelBarrier` requires `current_level` reset when `inject_tasks()` inserts tasks below the current level
- NEVER re-derive max_parallel without syncing `self.max_parallel` — slot drift is a liveness bug

---

## Plan Verification

`PlanVerifier<P>` — LLM-based completeness check after task completion. Issue #2202.

### Verification Flow

After the last task in a plan completes, `PlanVerifier.verify()` is called:
1. Returns `VerificationResult { complete, gaps: Vec<Gap>, confidence }`
2. `Gap { description, severity: GapSeverity, suggested_task }`
3. `GapSeverity`: `Critical`, `Important`, `Minor`
4. If `complete = false` and non-minor gaps exist: `replan()` is called to inject new tasks
5. `inject_tasks()` validates acyclicity and marks newly ready tasks

### Replan Constraints

- `max_tasks` cap: replan respects global task limit
- Minor-only gaps: `replan()` is skipped — minor gaps don't justify extra LLM calls
- `max_replans` per-task cap: second `inject_tasks()` call for the same task is a silent no-op
- Global `max_replans`: enforced across the whole scheduler — prevents infinite verify→replan loops
- `replan_prompt` gap descriptions truncated to 500 chars to limit injection blast radius

### Fail-Open Behavior

LLM error during `verify()` → treated as `complete = true` (fail-open). Consecutive failure tracking: `ERROR` log emitted at ≥ 3 consecutive failures.

### Config

```toml
[orchestration]
verify_completeness = true
verify_provider = "quality"           # must exist in [[llm.providers]]
completeness_threshold = 0.7          # confidence threshold for "complete" verdict [0.0, 1.0]
max_replans_remaining = 3             # global per-plan replan budget (VMAO)
```

`verify_provider` is validated at `DagScheduler` construction. Empty string = fallback to primary. Unknown provider name = `Err(InvalidConfig)` (hard fail).

`completeness_threshold` (default 0.7): when the verifier's `confidence` field is below this value, the plan is treated as incomplete even if `complete = true`. This handles uncertain LLM verdicts.

`max_replans_remaining` is initialized per plan and decremented on each successful replan. When it reaches zero, no further replanning occurs regardless of gap severity.

### VMAO: Verify-and-Modify Adaptive Orchestration

VMAO (Verify-and-Modify Adaptive Orchestration) extends Plan Verification with adaptive replanning:

1. **`verify_plan()`** — called after each task completes (not only at plan end)
   - Returns `VerificationResult` with `complete`, `gaps`, `confidence`
   - When `confidence < completeness_threshold` AND incomplete → trigger replan
   - When `confidence >= completeness_threshold` AND complete → skip replan
2. **`replan_from_plan()`** — injects new tasks from gap descriptions into the existing DAG
   - Respects `max_replans_remaining` per plan
   - New tasks are injected via `inject_tasks()` with acyclicity validation
   - Replan prompt gap descriptions truncated to 500 chars (blast radius limit)

`DagScheduler` gains:
- `completeness_threshold: f64` — configurable confidence threshold
- `verify_provider_name: Option<String>` — provider for verification calls
- `max_replans_remaining: u32` — mutable countdown, decremented per replan

### Key Invariants

- Fail-open on LLM error — never block task completion on verifier failure
- Minor-only gaps never trigger replan
- `inject_tasks()` must validate acyclicity — never add a cycle to the DAG
- Gap descriptions are sanitized via `ContentSanitizer` before prompt embedding
- `verify_provider` must be validated at construction, not at verify time
- NEVER emit `SchedulerAction::Verify` when `verify_completeness = false`
- `max_replans_remaining = 0` means no replanning; do not decrement below zero
- `completeness_threshold` must be in `[0.0, 1.0]` — values outside are a config error
- `verify_plan()` and `replan_from_plan()` are called per-task, not only at plan end (VMAO)
- NEVER block task dispatch while verification is in progress — verification is async

---

## ExecutionMode per Task

`ExecutionMode` annotation on `TaskNode`. Issue #2172.

LLM planner marks each task as `parallel` or `sequential`. `DagScheduler.tick()` serializes sequential tasks: at most one sequential task is dispatched at a time (others wait). `serde(default)` ensures backward compatibility with SQLite-stored graphs without this field.

### Key Invariants

- Sequential tasks must serialize within their ready set — never dispatch two sequential tasks simultaneously
- `ExecutionMode` defaults to `parallel` for graphs loaded without the field

---

## Cascade-Aware DAG Routing


`CascadeDetector` tracks failure rates per root-anchored region. When a region's failure rate exceeds `cascade_failure_threshold`, tasks in that region are deprioritized in the ready queue so healthy branches run first. Resets on `inject_tasks()`.

### Config

```toml
[orchestration]
cascade_routing = false
cascade_failure_threshold = 0.5
topology_selection = true   # required for CascadeAware dispatch strategy
```

### Key Invariants

- `DispatchStrategy::CascadeAware` requires `topology_selection = true` — startup warning emitted otherwise
- Cascade detection resets to zero on `inject_tasks()` — failure rates do not persist across plan restarts
- Deprioritized tasks are still dispatched eventually — this is ordering, not blocking

---

## Tree-Optimized Dispatch


`DispatchStrategy::TreeOptimized` sorts the ready queue by critical-path distance (deepest tasks first) for `FanOut`/`FanIn` topologies.

### Config

```toml
[orchestration]
tree_optimized_dispatch = false
```

### Key Invariants

- `TreeOptimized` applies only to `FanOut`/`FanIn` topologies — no-op for `Linear`/`Mixed`
- Critical-path distance is computed at dispatch time, not at plan creation
- NEVER assume `ExecutionMode::Sequential` implies dependency — it only controls concurrency

---

## VeriMAP Predicate Gate

Issue #3097. `VeriMAP` (Verify, Map, and Prune) is a predicate-gate layer that runs before task dispatch. Each `TaskNode` may carry a TOML-serialized predicate expression evaluated against the current plan state. Tasks whose predicate evaluates to `false` are skipped (not aborted) for the current tick.

### Predicate Expressions

Predicates are boolean expressions over plan state variables:

| Variable | Type | Description |
|----------|------|-------------|
| `completed(task_id)` | bool | Task completed successfully |
| `failed(task_id)` | bool | Task failed (any failure) |
| `running_count` | usize | Number of currently running tasks |
| `pending_count` | usize | Number of pending tasks |

Expressions combine with `&&`, `||`, `!`, and parentheses.

### Config

```toml
[orchestration]
verimap_enabled = false   # opt-in
```

### Key Invariants

- VeriMAP predicate evaluation runs before topology-based dispatch — a task blocked by predicate is re-evaluated on the next tick
- Predicate evaluation is pure (no side effects) — it only reads plan state
- Parse errors at task creation time are hard errors — a task with an invalid predicate expression is rejected at plan construction, not at dispatch
- NEVER abort a task based on a predicate — only skip for the current tick

---

## AdaptOrch Topology Advisor

Issue #3099. `AdaptOrch` is a bandit-driven topology advisor that runs before `LlmPlanner`. A 16-arm Thompson Beta-bandit (4 task classes × 4 topology hints) learns which topology hint produces better plans for each goal class.

### `TopologyAdvisor`

`TopologyAdvisor::recommend(goal_text)` classifies the goal into a `TaskClass` and samples a `TopologyHint`:

| TaskClass | Description |
|-----------|-------------|
| `IndependentBatch` | Fan-out work with no cross-dependencies (research, comparisons) |
| `SequentialPipeline` | Strict ordering: build→test→deploy, ETL |
| `HierarchicalDecomp` | Tree decomposition, recursive analysis |
| `Unknown` | Fallback; defaults to `Hybrid` hint |

| TopologyHint | Prompt sentence injected |
|--------------|--------------------------|
| `Parallel` | Prefer maximizing parallel tasks |
| `Sequential` | Produce a strict linear chain |
| `Hierarchical` | Decompose into subgoals with 2–3 depth levels |
| `Hybrid` | No constraint (no sentence injected) |

`record_outcome()` updates the Beta-bandit arm for the (class, hint) pair based on plan quality signals (task completion rate, verifier confidence). State is persisted at shutdown.

### Config

```toml
[orchestration]
adapt_orch_enabled = false   # opt-in
```

### Key Invariants

- `TopologyHint::Hybrid` injects no sentence — `prompt_sentence()` returns `None`
- Classification failure always produces `TaskClass::Unknown` with `TopologyHint::Hybrid` — no propagated error
- `record_outcome()` is synchronous — never spawns a background task
- Bandit state persists between sessions — NEVER reset without explicit user action
- `TopologyAdvisor` is advisory only — `TopologyClassifier::analyze()` still runs on the produced graph and may override the hint

---

## CoE (Cascade of Experts) Entropy Routing

Issue #3099. `CoE` routes each sub-plan task to the provider whose entropy profile best matches the task's complexity signal. Entropy is estimated from the task description length, dependency depth, and past latency.

### Config

```toml
[orchestration]
coe_routing_enabled = false   # opt-in
coe_routing_provider = ""     # fallback when CoE routing is disabled; empty = planner_provider
```

### Key Invariants

- `coe_routing_enabled = false` falls back to `planner_provider` for all tasks — no behavioral change
- CoE routing is per-task, not per-plan — different tasks in the same plan may route to different providers

---

## Graph Persistence in Scheduler Loop

Issue #3107 / #3124. `GraphPersistence::save()` is called from within the `DagScheduler` tick loop after each task state transition. This ensures the graph state is durable across scheduler restarts without requiring a separate flush-on-shutdown step.

### Key Invariants

- `GraphPersistence::save()` is called after every task state transition in `DagScheduler::tick()` — not only at shutdown
- Save failures are non-fatal — they are logged at `WARN` level and the scheduler continues
- NEVER call `save()` in the hot path before task dispatch — only after state has actually changed

---

## CascadeDetector Forward Adjacency Cache

Issue #3114. `CascadeDetector` caches the forward adjacency set (direct successors of each task node) to avoid repeated O(E) graph traversal during every tick.

### Key Invariants

- Cache is invalidated on `inject_tasks()` — new task injection resets the adjacency index
- NEVER use a stale adjacency cache after `inject_tasks()` — must rebuild before next tick

---

## Cascade Abort Defense

Error cascade defense (arXiv:2603.04474) aborts a DAG when consecutive failures in a
`depends_on` chain exceed the configured threshold, preventing silent propagation of
a root failure through the entire graph.

Two independent abort signals are evaluated after every `TaskOutcome::Failed` event:

1. **Linear-chain abort**: `cascade_chain_threshold` consecutive `Failed` entries in a
   `depends_on` path trigger abort. The chain is built by merging parent lineage entries
   into the failing task's chain.

2. **Fan-out rate abort**: when `cascade_failure_rate_abort_threshold > 0.0` and a region's
   failure rate reaches the threshold (with ≥ 3 tasks observed), the DAG is aborted.
   Requires `cascade_routing = true`.

Lineage is stored as a **side-table on `DagScheduler`** (`lineage_chains: HashMap<TaskId, ErrorLineage>`),
not on `TaskNode` — avoiding database serialization cost.

### Config

```toml
[orchestration]
cascade_chain_threshold = 3               # 0 = disable chain abort; must not be 1
cascade_failure_rate_abort_threshold = 0.0  # 0.0 = disable; recommended production: 0.7
lineage_ttl_secs = 300                    # must be > 0
```

### Key Invariants

- `cascade_chain_threshold = 1` is rejected at config validation — it would abort on every failure
- `lineage_ttl_secs = 0` is rejected — use `cascade_chain_threshold = 0` to disable lineage
- `cascade_failure_rate_abort_threshold` must be in `[0.0, 1.0]`; `0.0` disables fan-out abort
- Lineage chains are reset on `inject_tasks()` — stale chains do not affect post-replan execution
- Fan-out abort requires `region_size >= 3`; single-failure 100%-rate regions never trigger abort
- Both signals (`chain_threshold` and `fan_out_rate`) are evaluated independently; first to fire wins
- NEVER store lineage on `TaskNode` or serialize it to the database — lineage is a runtime-only signal
- Audit log MUST emit ONE structured `tracing::error!` per abort with `root`, `chain_depth`, and `cause`

