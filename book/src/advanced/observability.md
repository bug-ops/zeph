# Observability & Cost Tracking

## OpenTelemetry Export

Zeph can export traces via OpenTelemetry (OTLP/gRPC). Feature-gated behind `otel`.

```bash
cargo build --release --features otel
```

### Configuration

```toml
[observability]
exporter = "otlp"                        # "none" (default) or "otlp"
endpoint = "http://localhost:4317"       # OTLP gRPC endpoint
```

### Spans

| Span | Attributes |
|------|------------|
| `llm.turn_call` | `model`, `provider` |
| `tool_exec` | `tool_name` |

Traces flush gracefully on shutdown. Point `endpoint` at any OTLP-compatible collector (Jaeger, Grafana Tempo, etc.).

## Cost Tracking

Per-model cost tracking with daily budget enforcement.

### Configuration

```toml
[cost]
enabled = true
max_daily_cents = 500   # Daily spending limit in cents (USD)
```

### Built-in Pricing

| Model | Input (per 1M tokens) | Output (per 1M tokens) |
|-------|----------------------|------------------------|
| Claude Sonnet | $3.00 | $15.00 |
| Claude Opus | $15.00 | $75.00 |
| GPT-4o | $2.50 | $10.00 |
| GPT-4o mini | $0.15 | $0.60 |
| GPT-5 mini | $0.25 | $2.00 |
| Ollama (local) | Free | Free |

Budget resets at UTC midnight. When `max_daily_cents` is reached, LLM calls are blocked until the next reset.

Current spend is exposed as `cost_spent_cents` in `MetricsSnapshot` and visible in the TUI dashboard.

### Per-Provider Cost Breakdown

`CostTracker` records token usage per provider name alongside the aggregate totals. Cache pricing is applied automatically per provider type (Claude: cache read = 10% of prompt, cache write = 125%; OpenAI: cache read = 50%; others: 0%).

The `/status` CLI command renders a per-provider table when cost tracking is enabled:

```
Provider         Input    Cache R   Cache W   Output    Cost ($)   Reqs
─────────────────────────────────────────────────────────────────────────
claude           12 500      4 200     1 100    3 200    0.0043      8
openai            5 000      2 000         0    1 500    0.0012      3
```

The same table is available in the TUI via the `/cost` command. Providers are sorted by cost descending. The breakdown resets alongside the daily spending total at UTC midnight.

`MetricsSnapshot.provider_cost_breakdown` exposes the per-provider data for programmatic access.

### Token Counting

Completion token counts use the `output_tokens` field from the API response (OpenAI, Ollama, and Compatible providers). Streaming paths retain a byte-length heuristic (`response.len() / 4`) as a fallback when the provider returns no usage data. Structured-output calls (`chat_typed`) also record usage so `eval_budget_tokens` enforcement reflects real token counts.

### Cost Per Successful Task (CPS)

CPS measures the average cost of reaching a successful agent turn (one where the LLM responded without errors). This metric is more meaningful than raw token cost because it factors in failed turns, retries, and provider switching.

The `/cost` command displays CPS alongside token costs:

```
Cost per successful task: $0.0089 (123 successful turns, $1.09 total)
```

CPS resets daily at UTC midnight alongside the cost budget. Use it to track whether your agent is becoming more or less efficient over time.

**In code:** access via `MetricsSnapshot.cost_cps_cents` and `MetricsSnapshot.cost_successful_tasks`.

## TaskSupervisor Metrics

Zeph uses a `TaskSupervisor` to manage background tasks (embedding, memory consolidation, file watching, etc.). Task metrics provide CPU and wall-time measurement for performance debugging.

### Enabling Task Metrics

Enable the optional `task-metrics` feature (included in `full`):

```bash
cargo build --release --features task-metrics
```

When enabled, each supervised task records:

- **Wall-time**: elapsed time from spawn to completion
- **CPU-time**: actual CPU cycles spent (OS-level thread time measurement)

Zero overhead when disabled — the feature gate compiles out the measurement code.

### Viewing Task Metrics

**In the TUI**, open the task registry via command palette:

```
Ctrl+P -> /tasks
```

Shows a live table of all active/completed tasks:

| Column | Meaning |
|--------|---------|
| Name | Task identifier (e.g., `chunk_file_42`, `memory_eviction`) |
| State | Running / Waiting / Completed / Aborted |
| Uptime | Seconds since last restart |
| Restarts | Number of times task has restarted |

**In Jaeger traces**, task metrics appear as span attributes:

- `task.wall_time_ms` — total elapsed time
- `task.cpu_time_ms` — CPU time actually spent
- `task.name` — task identifier

**Via metrics export**, histograms are emitted to OTLP:

```
zeph.task.wall_time_ms    # milliseconds
zeph.task.cpu_time_ms     # milliseconds
```

Use `tokio-console` for real-time task monitoring when connecting to a running Zeph instance.

### Example: Debugging Slow Indexing

If code indexing is slow, check the task registry:

```
Name              State   Uptime  Restarts
────────────────────────────────────────────
chunk_file_12     Done    2345ms  0
chunk_file_13     Done    1890ms  0
chunk_file_14     Running 523ms   0
indexer_refresh   Done    5400ms  0
```

High wall-time with low CPU-time suggests I/O blocking (network, disk). High CPU-time suggests compute-heavy embedding. View the Jaeger trace for `chunk_file_14` to see where time is spent in the embedding pipeline.
