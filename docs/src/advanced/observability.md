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
| Ollama (local) | Free | Free |

Budget resets at UTC midnight. When `max_daily_cents` is reached, LLM calls are blocked until the next reset.

Current spend is exposed as `cost_spent_cents` in `MetricsSnapshot` and visible in the TUI dashboard.
