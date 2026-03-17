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
