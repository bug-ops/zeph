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
| `llm_call` | `model` |
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
