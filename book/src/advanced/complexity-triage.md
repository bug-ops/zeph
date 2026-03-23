# Complexity Triage Routing

Complexity triage routing (`routing = "triage"`) classifies each request before inference and routes it to the most appropriate provider tier based on difficulty. A cheap, fast model acts as the classifier; heavier models are reserved for genuinely difficult requests.

## How It Works

On each request the router:

1. Sends the user's message to the **triage provider** (a small, fast model).
2. The triage model returns a single word: `simple`, `medium`, `complex`, or `expert`.
3. The router looks up the configured provider for that tier and forwards the full request to it.
4. If triage times out or returns an unparseable response, the request falls back to the lowest configured tier (simple).

Context size is also considered: when a request's message history exceeds the selected tier provider's context window, the router automatically escalates to the next tier. This escalation count is tracked in the triage metrics.

### Tier Definitions

| Tier | Typical requests |
|------|-----------------|
| `simple` | Short factual questions, greetings, one-liners |
| `medium` | Summarization, translation, structured extraction |
| `complex` | Multi-step reasoning, code generation, analysis |
| `expert` | Research-grade tasks, long-form synthesis, advanced mathematics |

## Enabling Triage Routing

Set `routing = "triage"` in `[llm]` and add a `[llm.complexity_routing]` section:

```toml
[llm]
routing = "triage"

[llm.complexity_routing]
enabled = true
triage_provider = "fast"
bypass_single_provider = true
triage_timeout_secs = 5

[llm.complexity_routing.tiers]
simple = "fast"
medium = "default"
complex = "smart"
expert = "expert"

[[llm.providers]]
name = "fast"
type = "ollama"
model = "qwen3:1.7b"

[[llm.providers]]
name = "default"
type = "ollama"
model = "qwen3:8b"
default = true

[[llm.providers]]
name = "smart"
type = "claude"
model = "claude-haiku-4-5-20251001"

[[llm.providers]]
name = "expert"
type = "claude"
model = "claude-sonnet-4-6"
```

Each tier value must match a `name` field in one of the `[[llm.providers]]` entries. Tiers are optional — any omitted tier resolves to the first configured tier provider (simple).

## Bypass Optimization

When `bypass_single_provider = true` (the default) and all configured tiers resolve to the same provider name, the triage call is skipped entirely. This avoids a redundant LLM call when, for example, only two tiers are configured and both point to the same model:

```toml
[llm.complexity_routing.tiers]
simple  = "fast"
medium  = "fast"   # same provider — triage is bypassed
complex = "smart"
# expert not set — resolves to "fast" (first tier)
```

> [!NOTE]
> Bypass is evaluated at construction time. Changing tier assignments requires a config reload or restart.

## Timeout and Fallback

The triage call is bounded by `triage_timeout_secs` (default: 5 seconds). When the triage model does not respond in time or returns an unrecognised label, the router falls back to the `simple` tier provider and increments the `timeout_fallbacks` metric counter.

```toml
[llm.complexity_routing]
triage_provider = "fast"
triage_timeout_secs = 3   # fail fast on slow local model
```

## Hybrid Mode: Triage + Cascade

Setting `fallback_strategy = "cascade"` enables hybrid routing: triage selects the initial tier, and cascade quality escalation is applied on top. If the selected tier provider returns a degenerate response (empty, repetitive, incoherent), the router escalates to the next tier automatically.

```toml
[llm.complexity_routing]
triage_provider = "fast"
fallback_strategy = "cascade"

[llm.complexity_routing.tiers]
simple  = "fast"
medium  = "default"
complex = "smart"
expert  = "expert"
```

> [!NOTE]
> `fallback_strategy = "cascade"` is the only supported value. This option is reserved for future expansion.

## Configuration Reference

`[llm.complexity_routing]` fields (active when `routing = "triage"`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `triage_provider` | string? | — | Pool entry `name` of the fast classifier model. Required when `bypass_single_provider` is false. |
| `bypass_single_provider` | bool | `true` | Skip triage when all tier mappings resolve to the same provider name. |
| `triage_timeout_secs` | u64 | `5` | Timeout for the triage classification call in seconds. On timeout, falls back to the simple tier. |
| `max_triage_tokens` | usize | `50` | Maximum output tokens allowed in the triage response. |
| `fallback_strategy` | string? | — | Set to `"cascade"` to enable hybrid triage + quality escalation. |

`[llm.complexity_routing.tiers]` fields:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `simple` | string? | — | Provider `name` for trivial requests. Used as the fallback provider on triage failure. |
| `medium` | string? | — | Provider `name` for moderate requests. |
| `complex` | string? | — | Provider `name` for multi-step or code-heavy requests. |
| `expert` | string? | — | Provider `name` for research-grade or highly complex requests. |

All tier fields are optional. Unset tiers fall back to `simple`; if `simple` is also unset, the first `[[llm.providers]]` entry is used.

## Metrics

The triage router exposes counters accessible via the TUI metrics panel and the debug log:

| Counter | Description |
|---------|-------------|
| `calls` | Total triage classification calls made |
| `tier_simple` | Requests routed to `simple` |
| `tier_medium` | Requests routed to `medium` |
| `tier_complex` | Requests routed to `complex` |
| `tier_expert` | Requests routed to `expert` |
| `timeout_fallbacks` | Classifications that timed out or failed to parse |
| `escalations` | Context-window auto-escalations |

## Known Limitations

- Triage accuracy depends entirely on the quality of the classifier model. A weak or poorly-prompted model may mislabel requests.
- The triage call adds latency before every request when bypass is not active. Use a locally hosted small model (e.g. `qwen3:1.7b` via Ollama) to keep overhead below 500 ms.
- Multiple concurrent Zeph instances share no triage state — each instance classifies independently.
