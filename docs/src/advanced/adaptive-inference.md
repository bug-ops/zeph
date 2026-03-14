# Adaptive Inference

When `provider = "router"`, Zeph routes each LLM request through a fallback chain of providers. The **routing strategy** determines which provider is tried first. Three strategies are available:

| Strategy | Config value | Description |
|----------|-------------|-------------|
| **EMA** (default) | `"ema"` | Latency-weighted exponential moving average. Reorders providers every N requests based on observed response times |
| **Thompson Sampling** | `"thompson"` | Bayesian exploration/exploitation via Beta distributions. Tracks per-provider success/failure counts and samples to choose the best provider |
| **Cascade** | `"cascade"` | Cost-escalation routing. Tries providers cheapest-first; escalates to the next provider only when the response is classified as degenerate (empty, repetitive, incoherent) |

## Thompson Sampling

Thompson Sampling maintains a Beta(alpha, beta) distribution per provider. On each request the router samples all distributions and picks the provider with the highest sample. After the request completes:

- **Success** (provider returns a response): alpha += 1
- **Failure** (provider errors, triggers fallback): beta += 1

New providers start with a uniform prior Beta(1, 1). Over time, reliable providers accumulate higher alpha values and get selected more often, while unreliable providers are deprioritized. The stochastic sampling ensures occasional exploration of underperforming providers in case they recover.

### Enabling Thompson Sampling

```toml
[llm]
provider = "router"

[llm.router]
chain = ["claude", "openai", "ollama"]
strategy = "thompson"
# thompson_state_path = "~/.zeph/router_thompson_state.json"  # optional
```

### State Persistence

Thompson state is saved to disk on agent shutdown and restored on startup. The default path is `~/.zeph/router_thompson_state.json`.

- The file is written atomically (tmp + rename) with `0o600` permissions on Unix
- On startup, loaded values are clamped to `[0.5, 1e9]` and checked for finiteness to reject corrupt state files
- Providers removed from the `chain` config are pruned from the state file automatically
- Multiple concurrent Zeph instances will overwrite each other's state on shutdown (known pre-1.0 limitation)

Override the path:

```toml
[llm.router]
thompson_state_path = "/path/to/custom-state.json"
```

### Inspecting State

**CLI:**

```bash
# Show alpha/beta and mean success rate per provider
zeph router stats

# Use a custom state file
zeph router stats --state-path /path/to/state.json

# Reset to uniform priors (deletes the state file)
zeph router reset
```

Example output:

```
Thompson Sampling state: /Users/you/.zeph/router_thompson_state.json
Provider                            alpha     beta        Mean%
--------------------------------------------------------------
claude                              45.00     3.00        62.1%
ollama                              12.00     8.00        20.8%
openai                              30.00     5.00        17.1%
```

**TUI:**

Type `/router stats` in the TUI input or select "Show Thompson router alpha/beta per provider" from the command palette.

## EMA Strategy

The default EMA strategy tracks latency per provider and periodically reorders the chain so faster providers are tried first. Configure via the top-level `[llm]` fields:

```toml
[llm]
provider = "router"
router_ema_enabled = true
router_ema_alpha = 0.1          # smoothing factor, 0.0-1.0
router_reorder_interval = 10    # re-order every N requests

[llm.router]
chain = ["claude", "openai", "ollama"]
strategy = "ema"                # default, can be omitted
```

## Cascade Routing

The cascade strategy routes requests to the cheapest provider first and escalates only when the response is degenerate. This minimizes cost while maintaining quality.

### Enabling Cascade Routing

```toml
[llm]
provider = "router"

[llm.router]
chain = ["ollama", "claude"]   # cheapest first
strategy = "cascade"

[llm.router.cascade]
quality_threshold = 0.5        # score below this → escalate (default: 0.5)
max_escalations = 2            # max escalation steps per request (default: 2)
classifier_mode = "heuristic"  # "heuristic" (default) or "judge" (LLM-backed)
# max_cascade_tokens = 100000  # cumulative token cap across escalation levels (optional)
```

### Classifier Modes

| Mode | Description |
|------|-------------|
| `heuristic` | Detects degenerate outputs only (empty, repetitive, incoherent) without LLM calls |
| `judge` | LLM-based quality scoring; requires `summary_model` to be configured. Falls back to heuristic on failure |

### Behavior

- Network and API errors do **not** consume the escalation budget — only quality-based failures trigger escalation.
- When all escalation levels are exhausted, the best-seen response is returned (not an error).
- Cascade is intentionally skipped for `chat_with_tools` calls (tool use requires deterministic provider selection).
- Thompson/EMA outcome tracking is not contaminated by quality-based escalations.

## Configuration Reference

Full `[llm.router]` section:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `chain` | string[] | required | Ordered list of provider names for fallback |
| `strategy` | `"ema"`, `"thompson"`, or `"cascade"` | `"ema"` | Routing strategy |
| `thompson_state_path` | string? | `~/.zeph/router_thompson_state.json` | Path for Thompson state persistence |

`[llm.router.cascade]` fields (when `strategy = "cascade"`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `quality_threshold` | float | `0.5` | Score below which the response is considered degenerate |
| `max_escalations` | int | `2` | Maximum escalation steps per request |
| `classifier_mode` | string | `"heuristic"` | `"heuristic"` or `"judge"` |
| `window_size` | int? | unset | Sliding window size for repetition detection |
| `max_cascade_tokens` | int? | unset | Cumulative token budget across escalation levels |

EMA-specific fields live in `[llm]`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `router_ema_enabled` | bool | `false` | Enable EMA latency tracking |
| `router_ema_alpha` | float | `0.1` | EMA smoothing factor |
| `router_reorder_interval` | int | `10` | Reorder interval in requests |

## Known Limitations

- Thompson success/failure is recorded at stream-open time, not on stream completion. A provider that opens a stream but fails mid-delivery still gets alpha += 1
- Multiple Zeph instances sharing the same state file will overwrite each other's state
- The state file uses a predictable `.tmp` suffix during writes (symlink-race risk on shared directories)
