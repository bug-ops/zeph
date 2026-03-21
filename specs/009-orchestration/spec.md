# Spec: Orchestration

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

## Key Invariants

- User confirmation (`/plan confirm`) is required before any task execution — never auto-start
- `LlmAggregator` must enforce per-task token budget — runaway tasks must be truncated
- `TaskGraph` must be a true DAG — cycles are a hard error, not a warning
- `DagScheduler` is tick-based (not event-driven) — tick interval is configurable
- Sub-agent results are merged by `LlmAggregator`, not concatenated — aggregation is an LLM call

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
