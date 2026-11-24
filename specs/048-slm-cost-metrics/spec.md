# Spec 048 — SLM Cost Metrics

**Source:** arXiv:2510.03847 — "Small Language Models for Agentic Systems" (October 2025)

## Overview

Small Language Models (1–12B parameters) are sufficient and often superior for schema-constrained
and API-constrained agentic tasks: tool calling, structured extraction, and RAG retrieval. The
capability gap with GPT-4-class models closes when guided decoding + strict JSON Schema output +
validator-first execution is applied. Cost reduction: 10x–100x lower token cost with materially
better latency and energy efficiency.

Applicable to Zeph in three areas:

1. **Orchestration** — `LlmPlanner` / task decomposition can use Qwen-2.5-7B or Phi-4-Mini as
   `planner_provider` at 10x lower cost with comparable quality on structured outputs.
2. **Compaction** — `fast` provider (GPT-4o-mini) can be replaced by locally-hosted Llama-3.2-3B
   via Ollama for zero-cost summarization.
3. **Sub-agent tool calling** — sub-agents running on Qwen-2.5-7B with guided decoding reduce
   cost significantly in orchestration-heavy workflows.

Surveyed models: Phi-4-Mini, Qwen-2.5-7B, Gemma-2-9B, Llama-3.2-1B/3B, Ministral-3B/8B,
DeepSeek-R1-Distill.

## CPS Metric

**Cost Per Successful Task (CPS)** measures the average cost in cents to complete one agent turn
that produces a usable response.

```
CPS = total_spent_cents / successful_tasks_today
```

A "successful task" is any agent turn that completes the LLM inference and returns a response
without error. The counter is incremented by `CostTracker::record_successful_task()`, which is
called immediately after `record_cost_and_cache()` at each turn completion point.

### API surface (`zeph-core::cost`)

| Method | Description |
|--------|-------------|
| `record_successful_task()` | Increments today's successful-task counter |
| `cps() -> Option<Cents>` | Returns CPS in cents; `None` until first task recorded |
| `successful_tasks() -> u64` | Returns count of successful tasks today |

### Metrics fields (`AgentMetrics`)

| Field | Type | Description |
|-------|------|-------------|
| `cost_cps_cents` | `Option<Cents>` | Current CPS value |
| `cost_successful_tasks` | `u64` | Successful task count today |

Both fields are updated in `record_cost_and_cache` → `record_successful_task` call chain inside
`utils.rs`.

## Key Invariants

- CPS and successful-task count reset at UTC day boundary, consistent with `spent_cents` reset.
- With `[cost] enabled = false` no `CostTracker` exists, so `cps()` is never available and `cost_cps_cents` stays `None`.
- A turn that errors before `record_cost_and_cache` is called is NOT counted as successful.
- Per-provider CPS is not tracked; only the global daily counter is maintained.

## Default Daily Cost Cap (#6469)

`CostConfig::max_daily_cents` — the daily cap enforced by the same `CostTracker`
(`crates/zeph-core/src/cost.rs`) this spec's CPS metric is computed from — now defaults to
`2500` ($25.00/day), changed from `0` (unlimited). A fresh install previously shipped with no
built-in defense against unbounded LLM spend. `0` remains a valid explicit override meaning
unlimited; existing configs with an explicit `max_daily_cents` value (including an explicit
`0`) are preserved — migration step 101 only appends an advisory comment, it never rewrites an
active value.

## Typed Money and Budget (#7176, #7192)

- Amounts are `zeph_common::money::Cents` and per-1000-token rates are `CentsPer1k` (spec 043 FR-018). `ModelPricing` carries four `CentsPer1k` rates; `CostTracker::price_of`, `current_spend`, `session_spend`, `cps` and `MetricsSnapshot.cost_spent_cents` / `cost_cps_cents` are `Cents`. `Channel::send_usage` takes `Cents` (the cumulative session cost). Config (`max_daily_cents` stays an integer), database and wire encodings are unchanged because the newtypes serialize transparently.
- `CostTracker::new(DailyBudget)` replaces the former enabled flag plus bare amount. `DailyBudget` is `Unlimited | Limited(Cents)` and `DailyBudget::from_config(max_daily_cents)` maps `0` to `Unlimited` at the config boundary. `is_exhausted(spent)` is true once spend reaches the limit and fails closed: a NaN or non-positive limit is always exhausted. `check_budget` returns `BudgetExhausted { spent_cents, budget_cents }` only for a `Limited` budget.
- NEVER compare a budget against `0.0` as an "unlimited" sentinel; use `DailyBudget` (#7176)

## SLM Provider Configuration

Add to `[[llm.providers]]` in config when Ollama is available locally:

```toml
[[llm.providers]]
name = "slm-medium"
type = "ollama"
base_url = "http://localhost:11434"
model = "qwen2.5:7b"
max_tokens = 8192
```

Reference this provider via `planner_provider = "slm-medium"` in `[orchestration]` or
`compaction_provider = "slm-medium"` in `[memory.compression]` for cost-optimised deployments.

## NEVER

- Never increment `successful_tasks` for turns that returned an LLM error or budget exhaustion.
- Never count turns that completed compaction or background tasks only (no user-facing response).
- Never remove the daily reset for `successful_tasks` — CPS is a daily operational metric, not a
  session-lifetime metric.

## Related Issues

- #2192 — SLMs are the Future of Agentic AI
- #2165 — Unified routing+cascading framework
- #2185 — Candle-backed lightweight classifiers
- #6469 — Enable runaway tool-call rate limiting and daily cost cap by default
