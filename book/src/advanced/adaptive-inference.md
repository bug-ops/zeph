# Adaptive Inference

When multiple providers are configured and `routing` is set in `[llm]`, Zeph routes each LLM request through the provider list. The **routing strategy** determines which provider is tried first. Four strategies are available:

| Strategy | Config value | Description |
|----------|-------------|-------------|
| **EMA** (default) | `"ema"` | Latency-weighted exponential moving average. Reorders providers every N requests based on observed response times |
| **Thompson Sampling** | `"thompson"` | Bayesian exploration/exploitation via Beta distributions. Tracks per-provider success/failure counts and samples to choose the best provider |
| **Cascade** | `"cascade"` | Cost-escalation routing. Tries providers cheapest-first; escalates to the next provider only when the response is classified as degenerate (empty, repetitive, incoherent) |
| **Complexity Triage** | `"triage"` | Pre-inference classification routing. A cheap triage model classifies each request as `simple`, `medium`, `complex`, or `expert` and delegates to the matching tier provider. See [Complexity Triage Routing](complexity-triage.md) |
| **Bandit** | `"bandit"` | PILOT LinUCB contextual bandit. Embeds each request and selects the provider that maximizes the upper confidence bound given observed cost-weighted rewards. See [Bandit Routing](#bandit-routing) |

## Thompson Sampling

Thompson Sampling maintains a Beta(alpha, beta) distribution per provider. On each request the router samples all distributions and picks the provider with the highest sample. After the request completes:

- **Success** (provider returns a response): alpha += 1
- **Failure** (provider errors, triggers fallback): beta += 1

New providers start with a uniform prior Beta(1, 1). Over time, reliable providers accumulate higher alpha values and get selected more often, while unreliable providers are deprioritized. The stochastic sampling ensures occasional exploration of underperforming providers in case they recover.

### Enabling Thompson Sampling

```toml
[llm]
routing = "thompson"
# thompson_state_path = "~/.zeph/router_thompson_state.json"  # optional

[[llm.providers]]
name = "claude"
type = "claude"
model = "claude-sonnet-4-6"

[[llm.providers]]
name = "openai"
type = "openai"
model = "gpt-4o"

[[llm.providers]]
name = "ollama"
type = "ollama"
model = "qwen3:8b"
```

### State Persistence

Thompson state is saved to disk on agent shutdown and restored on startup. The default path is `~/.zeph/router_thompson_state.json`.

- The file is written atomically (tmp + rename) with `0o600` permissions on Unix
- On startup, loaded values are clamped to `[0.5, 1e9]` and checked for finiteness to reject corrupt state files
- Providers removed from the `chain` config are pruned from the state file automatically
- Multiple concurrent Zeph instances will overwrite each other's state on shutdown (known pre-1.0 limitation)

Override the path:

```toml
[llm]
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
routing = "ema"
router_ema_enabled = true
router_ema_alpha = 0.1          # smoothing factor, 0.0-1.0
router_reorder_interval = 10    # re-order every N requests

[[llm.providers]]
name = "claude"
type = "claude"
model = "claude-sonnet-4-6"

[[llm.providers]]
name = "openai"
type = "openai"
model = "gpt-4o"

[[llm.providers]]
name = "ollama"
type = "ollama"
model = "qwen3:8b"
```

## Cascade Routing

The cascade strategy routes requests to the cheapest provider first and escalates only when the response is degenerate. This minimizes cost while maintaining quality.

### Enabling Cascade Routing

```toml
[llm]
routing = "cascade"

[llm.cascade]
quality_threshold = 0.5        # score below this → escalate (default: 0.5)
max_escalations = 2            # max escalation steps per request (default: 2)
classifier_mode = "heuristic"  # "heuristic" (default) or "judge" (LLM-backed)
# max_cascade_tokens = 100000  # cumulative token cap across escalation levels (optional)
# cost_tiers = ["ollama", "claude"]  # explicit cost ordering (cheapest first)

[[llm.providers]]
name = "ollama"
type = "ollama"
model = "qwen3:8b"

[[llm.providers]]
name = "claude"
type = "claude"
model = "claude-sonnet-4-6"
```

#### `cost_tiers`

`cost_tiers` lets you override the escalation order without changing the `[[llm.providers]]` list order. It is applied once at construction time (no per-request cost). Providers listed in `cost_tiers` are reordered to match that sequence; any provider not mentioned is appended after the listed ones in the original order. Unknown names in `cost_tiers` are silently ignored.

```toml
[llm.cascade]
cost_tiers = ["ollama", "openai"]  # reorder to cheapest first; claude appended last
```

This separates the fallback chain definition (used by all strategies) from the cost ordering used specifically by cascade.

> [!NOTE]
> `cost_tiers` only affects `chat_stream` / `chat` calls. `chat_with_tools` bypasses cascade entirely and uses the original chain order.

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

`[llm]` routing fields:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `routing` | `"none"`, `"ema"`, `"thompson"`, `"cascade"`, `"task"`, `"bandit"` | `"none"` | Routing strategy |
| `thompson_state_path` | string? | `~/.zeph/router_thompson_state.json` | Path for Thompson state persistence |
| `bandit_state_path` | string? | `~/.config/zeph/router_bandit_state.json` | Path for bandit state persistence |

`[llm.cascade]` fields (when `routing = "cascade"`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `quality_threshold` | float | `0.5` | Score below which the response is considered degenerate |
| `max_escalations` | int | `2` | Maximum escalation steps per request |
| `classifier_mode` | string | `"heuristic"` | `"heuristic"` or `"judge"` |
| `window_size` | int? | unset | Sliding window size for repetition detection |
| `max_cascade_tokens` | int? | unset | Cumulative token budget across escalation levels |
| `cost_tiers` | string[]? | unset | Explicit cost ordering (cheapest first); providers not listed are appended after listed ones in original order |

EMA-specific fields live in `[llm]`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `router_ema_enabled` | bool | `false` | Enable EMA latency tracking |
| `router_ema_alpha` | float | `0.1` | EMA smoothing factor |
| `router_reorder_interval` | int | `10` | Reorder interval in requests |

## Bandit Routing

The `"bandit"` strategy implements the PILOT LinUCB contextual bandit algorithm. Unlike Thompson Sampling (which tracks success/failure counts) or EMA (which tracks latency), the bandit embeds the current request as a feature vector and selects the provider that maximizes the upper confidence bound given observed cost-weighted rewards. This allows the router to learn which providers perform best for different *types* of requests, not just which provider is fastest or most reliable overall.

### How It Works

1. The incoming request is embedded using `embedding_provider` to produce a context vector.
2. Each provider maintains a LinUCB model: a ridge regression matrix and a reward vector.
3. The router computes a UCB score for every provider: the estimated reward plus an exploration bonus scaled by `alpha`.
4. The provider with the highest score handles the request.
5. After the request completes, the reward (quality signal minus cost penalty) is used to update that provider's model.
6. The `decay_factor` attenuates historical observations over time, allowing the bandit to adapt to changes in provider behavior.

### Enabling Bandit Routing

```toml
[llm]
routing = "bandit"

[llm.router.bandit]
alpha = 1.0                          # Exploration bonus coefficient (default: 1.0)
dim = 64                             # Embedding dimension for context features (default: 64)
cost_weight = 0.1                    # Weight applied to token cost in the reward signal (default: 0.1)
decay_factor = 0.99                  # Per-request exponential decay of historical observations (default: 0.99)
embedding_provider = "fast"          # Provider name to use for request embedding
embedding_timeout_ms = 500           # Timeout for the embedding call in milliseconds (default: 500)
cache_size = 256                     # LRU cache size for repeated request embeddings (default: 256)

[[llm.providers]]
name = "fast"
type = "openai"
model = "gpt-4o-mini"
embed = true

[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
```

### State Persistence

Bandit model state (the per-provider LinUCB matrices) is saved on agent shutdown and restored on startup. The default path is `~/.config/zeph/router_bandit_state.json`. Override with:

```toml
[llm]
bandit_state_path = "/path/to/custom-bandit-state.json"
```

The file is written atomically (tmp + rename) with `0o600` permissions on Unix. On startup, loaded matrices are validated for dimensionality consistency — mismatched dimensions (e.g., after changing `dim`) cause a clean reset to the uniform prior.

### Configuration Reference

`[llm.router.bandit]` fields (active when `routing = "bandit"`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `alpha` | float | `1.0` | Exploration bonus coefficient. Higher values favor exploration of less-tested providers |
| `dim` | usize | `64` | Embedding dimension. Must match the embedding model's output; changing this resets the state |
| `cost_weight` | float | `0.1` | Relative weight of token cost in the reward signal. Higher values penalize expensive providers more aggressively |
| `decay_factor` | float | `0.99` | Per-request multiplicative decay applied to historical observations. Values closer to 1.0 retain history longer |
| `embedding_provider` | string? | — | Provider `name` used to embed requests. Should reference a fast, cheap embedding-capable provider |
| `embedding_timeout_ms` | u64 | `500` | Timeout for the embedding call. On timeout, the bandit falls back to the first provider in the chain |
| `cache_size` | usize | `256` | LRU cache capacity for request embeddings. Repeated or similar requests reuse cached vectors |

### Inspecting State

```bash
# Show per-provider bandit statistics
zeph router stats --strategy bandit
```

The output includes the estimated reward mean and uncertainty per provider, the number of observations, and the current `alpha`/`decay_factor` parameters.

## Known Limitations

- Thompson success/failure is recorded at stream-open time, not on stream completion. A provider that opens a stream but fails mid-delivery still gets alpha += 1
- Multiple Zeph instances sharing the same state file will overwrite each other's state
- The state file uses a predictable `.tmp` suffix during writes (symlink-race risk on shared directories)
