# zeph-llm

[![Crates.io](https://img.shields.io/crates/v/zeph-llm)](https://crates.io/crates/zeph-llm)
[![docs.rs](https://img.shields.io/docsrs/zeph-llm)](https://docs.rs/zeph-llm)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

LLM provider abstraction with Ollama, Claude, OpenAI, Gemini, Gonka, and Candle backends.

## Overview

Defines the `LlmProvider` trait and ships concrete backends for Ollama, Claude, OpenAI, Google Gemini, Gonka (signed native transport), and OpenAI-compatible endpoints. Includes a `RouterProvider` that selects among backends via four strategies (EMA, Thompson sampling, cascade, LinUCB bandit), a typed structured-extraction layer (`Extractor` / `chat_typed`), an optional Candle backend for local inference, and a disk-backed cache for remote model listings.

## Key modules

| Module | Description |
|--------|-------------|
| `provider` | `LlmProvider` trait — unified inference interface; `name()` returns `&str` (no longer `&'static str`); `Message` carries `MessageMetadata`, whose `visibility: MessageVisibility` enum (`Both` / `AgentOnly` / `UserOnly`) replaces the former `(agent_visible, user_visible)` bool pair so the invalid "visible to nobody" state is unrepresentable |
| `ollama` | Ollama HTTP backend |
| `claude` | Anthropic Claude backend with `with_client()` builder for shared `reqwest::Client` |
| `openai` | OpenAI backend with `with_client()` builder for shared `reqwest::Client` |
| `gemini` | Google Gemini backend (`generateContent` + `streamGenerateContent?alt=sse`); system prompt mapped to `systemInstruction`, `assistant` role to `"model"`, consecutive same-role message merging, thinking parts surfaced as `StreamChunk::Thinking`, `functionCall` parts in SSE stream emitted as `StreamChunk::ToolUse`; configured via `[llm.gemini]` and `ZEPH_GEMINI_API_KEY` |
| `compatible` | Generic OpenAI-compatible endpoint backend |
| `gonka` | Gonka native inference backend — signed HTTP transport via `RequestSigner`, `EndpointPool` for weighted multi-node load balancing; supports `chat`, `chat_stream`, `embed`, and `chat_with_tools` (feature `gonka`) |
| `candle_provider` | Local inference via Candle (feature `candle`) |
| `any` | `AnyProvider` enum wrapping every backend for uniform dispatch; `set_thinking_budget()` / `apply_reasoning_effort()` / `current_thinking_budget()` / `current_reasoning_effort()` mutate the active provider's thinking/reasoning settings at runtime (session-only, never persisted), delegating through `Router`/`Triage` to the last-active inner provider |
| `router` | `RouterProvider` selects among backends via four strategies: EMA latency tracking, Thompson sampling (Beta distributions), cascade escalation, and LinUCB bandit. Providers stored as `Arc<[AnyProvider]>` — `clone()` on every LLM request is O(1) regardless of chain length |
| `extractor` | `Extractor` / `chat_typed<T>()` — typed LLM output via JSON Schema (`schemars`); per-`TypeId` schema caching |
| `sse` | Shared `sse_to_chat_stream()` helpers for Claude and OpenAI SSE parsing |
| `stt` | `SpeechToText` trait and `WhisperProvider` (OpenAI Whisper API) |
| `whisper` | Whisper model plumbing shared by the STT backends |
| `candle_whisper` | Local offline STT via Candle (whisper-tiny/base/small, feature `candle`) |
| `classifier` | Candle-backed classifiers and metrics (feature `classifiers`) |
| `model_cache` | Disk-backed cache for remote model listings (24-hour TTL) |
| `masking` | `MaskedProvider` / `OutboundMasker` — outbound secret masking |
| `http` | `default_client()` — shared HTTP client with standard timeouts and user-agent |
| `error` | `LlmError` — unified error type; `ContextLengthExceeded` variant with `is_context_length_error()` heuristic matching across provider error formats (Claude, OpenAI, Ollama) |

**Re-exports:** `LlmProvider`, `LlmProviderDyn`, `LlmError`, `Extractor`, `ChatStream`, `StreamChunk`, `CompatibleConfig`, `OpenAiConfig`, `MaskedProvider`, `SpeechToText`

## Router strategies

The router supports four strategies — EMA and Thompson sampling reorder the fallback chain (covered below); cascade and LinUCB bandit have their own sections further down. Set the strategy in `[llm.router]`:

### EMA (default)

Exponential moving average latency tracking. After each response, `EmaTracker` records provider latency and periodically reorders the chain so the fastest reliable provider is tried first.

```toml
[llm]
router_ema_enabled      = true
router_ema_alpha        = 0.1   # smoothing factor; lower = slower to adapt
router_reorder_interval = 60    # seconds between reordering

[llm.router]
strategy = "ema"
```

### Thompson Sampling

Adaptive model selection using Beta distributions. Each provider maintains a Beta(alpha, beta) distribution initialized with a uniform prior (1, 1). On each request the router samples all distributions and picks the provider with the highest sample; after the response it updates alpha (success) or beta (failure). This naturally balances exploration of less-tested providers with exploitation of known-good ones.

State persists across restarts to `~/.zeph/router_thompson_state.json` (configurable). Stale entries for removed providers are pruned automatically on startup.

```toml
[llm.router]
chain    = ["claude", "openai", "ollama"]
strategy = "thompson"
# thompson_state_path = "~/.zeph/router_thompson_state.json"  # optional
```

CLI commands for inspecting and managing Thompson state:

```bash
zeph router stats   # show per-provider alpha/beta and success rate
zeph router reset   # reset all distributions to uniform prior
```

TUI: `/router stats` displays the same information in the dashboard.

> [!NOTE]
> Thompson Sampling is most useful when you have multiple providers with varying reliability and want the router to automatically converge on the best one while still occasionally probing alternatives.

## Cascade routing

The cascade strategy tries providers in order and escalates to the next when a quality threshold is not met. Configure via `[llm.router.cascade]`:

```toml
[llm.router]
strategy = "cascade"
chain = ["ollama", "claude", "openai"]

[llm.router.cascade]
quality_threshold = 0.7
max_escalations = 2
cost_tiers = ["ollama", "claude", "openai"]  # optional: explicit cheapest-first ordering
```

`cost_tiers` reorders providers once at construction time (zero per-request cost). Providers absent from the list are appended after listed ones in original chain order. Unknown names are silently ignored.

## Complexity triage routing

The triage strategy classifies each request into a complexity tier before inference and routes it to the provider pool configured for that tier. This avoids sending simple queries to expensive models and reserves high-capability models for genuinely complex tasks.

```toml
[llm.router]
strategy = "triage"

[llm.complexity_routing]
simple_providers  = ["ollama"]
medium_providers  = ["ollama", "openai"]
complex_providers = ["claude", "openai"]
expert_providers  = ["claude"]
```

Tier assignment uses a lightweight classifier (`TriageClassifier`) that runs before the primary LLM call. The classifier dispatches to `LlmRoutingStrategy::Triage` on the `RouterProvider`.

> [!TIP]
> Use `ClassifierMode::Judge` to route classification through a separate LLM call when heuristic scoring is insufficient for your workload.

## PILOT LinUCB bandit routing

The `bandit` strategy applies a contextual LinUCB bandit to provider selection. On each request, context features (query complexity score, recent per-provider latency, time-of-day bucket) are assembled into a feature vector; the bandit computes an upper confidence bound per provider and selects the highest. After each response, the reward signal (success × inverse latency) updates the ridge regression weights.

State is persisted to `~/.zeph/router_bandit_state.json` (configurable) and restored on restart.

```toml
[llm.router]
strategy = "bandit"
chain    = ["ollama", "claude", "openai"]

[llm.router.bandit]
alpha            = 1.0     # exploration parameter; higher = more exploration
state_path       = "~/.zeph/router_bandit_state.json"
feature_dim      = 8       # dimensionality of the context feature vector
```

> [!NOTE]
> PILOT (Provider Intelligent Linucb Online Tracking) is most effective when providers have meaningfully different latency/quality profiles and the workload has varied query complexity. For uniform workloads, Thompson Sampling may converge faster.

> [!TIP]
> Inspect learned weights and UCB scores with `zeph router stats` (same command as Thompson Sampling) or `/router stats` in the TUI.

## Gonka native provider

`GonkaProvider` connects to Gonka inference nodes using a signed transport. Requests are signed per-call via `RequestSigner` using an HMAC key stored in the age vault under `ZEPH_GONKA_API_KEY`. `EndpointPool` distributes load across nodes by weight and falls back automatically when a node is unreachable.

Supported operations: `chat`, `chat_stream`, `embed`, `chat_with_tools`, and `chat_typed` (structured output via JSON Schema).

```toml
[[llm.providers]]
name = "gonka"
type = "gonka"
model = "qwen3-235b"
default = true

[[llm.gonka_nodes]]
url    = "https://node.example.gonka.ai"
weight = 1
```

Store the key in the vault:

```bash
zeph vault set ZEPH_GONKA_API_KEY <your-key>
```

Configure via the `--init` wizard by selecting the **GonkaGate / Gonka Native** option.

> [!NOTE]
> The GonkaGate path (`type = "compatible"`) is still available for access via the OpenAI-compatible gateway. Use `type = "gonka"` for the native signed-transport path with full multi-node pool support.

## SLM provider recommendations

For cost-sensitive or resource-constrained deployments, the following Small Language Models are verified to work well with Zeph:

| Task | Recommended SLM | Notes |
|------|----------------|-------|
| Embeddings | `qwen3-embedding` (Ollama) | Default embedding model |
| Simple queries / routing | `qwen3:8b` (Ollama) | Fast, low memory footprint |
| Summarization / compaction | `qwen3:8b` or `phi-4-mini` | Good quality at 8B scale |
| Graph extraction | `qwen3:8b` | Structured output via JSON Schema |
| STT | `whisper-tiny` / `whisper-base` (Candle) | Local offline, no API key |

Pair SLMs with a cloud provider for complex/expert tasks using triage routing:

```toml
[llm.router]
strategy = "triage"

[llm.complexity_routing]
simple_providers  = ["ollama"]   # qwen3:8b handles simple queries
medium_providers  = ["ollama"]
complex_providers = ["claude"]
expert_providers  = ["claude"]
```

## Claude extended thinking

`ClaudeProvider` supports two thinking modes via `ThinkingConfig`:

| Mode | Description |
|------|-------------|
| `Extended { budget_tokens }` | Allocates a fixed token budget (1024–128000) for visible reasoning; emits `interleaved-thinking-2025-05-14` beta header on legacy Sonnet 4.6 with tools (current models use adaptive thinking and need no beta header) |
| `Adaptive { effort? }` | Lets the model allocate thinking budget automatically |

```toml
[llm.claude]
thinking = { mode = "extended", budget_tokens = 16000 }
```

CLI: `--thinking extended:16000` or `--thinking adaptive`. When thinking is enabled and `max_tokens` is below 16000, it is raised automatically. Thinking deltas are parsed from the SSE stream and suppressed from the user-facing output; `MessagePart::ThinkingBlock` variants preserve thinking blocks verbatim across tool-use turns.

The thinking budget and OpenAI/Compatible/Gemini `reasoning_effort` can also be changed mid-session via `AnyProvider::set_thinking_budget()` / `apply_reasoning_effort()` — surfaced as the `/think-tokens [N|Nk|NM|off]` and `/reasoning-effort [low|medium|high]` slash commands (and a matching `--reasoning-effort` CLI flag / `--init` wizard prompt). Overrides are session-only: never persisted across restarts or `/provider` switches, and unsupported providers return an explicit "not supported" message instead of a silent no-op.

## Prompt cache TTL

`ClaudeProvider` supports a configurable prompt cache TTL via the `CacheTtl` enum:

| Variant | TTL | Header |
|---------|-----|--------|
| `Ephemeral` (default) | ~5 minutes | standard `cache_control` |
| `OneHour` | 1 hour | `extended-cache-ttl-2025-04-25` beta |

```toml
[[llm.providers]]
type = "claude"
model = "claude-sonnet-5"
prompt_cache_ttl = "1h"   # "ephemeral" (default) or "1h"
```

> [!NOTE]
> The 1-hour TTL costs approximately 2× more per cache write but dramatically reduces repeated-prefix costs for long sessions. Default `"ephemeral"` is byte-identical to the previous wire format — no rollout risk for existing deployments.

## Gemini configuration

```toml
[llm]
provider = "gemini"

[llm.gemini]
model = "gemini-2.0-flash"   # or "gemini-2.5-pro" for extended thinking
max_tokens = 8192
# base_url = "https://generativelanguage.googleapis.com/v1beta"
```

Store the API key in the vault: `zeph vault set ZEPH_GEMINI_API_KEY AIza...`

> [!NOTE]
> Gemini does not expose an embeddings endpoint. For semantic memory and skill matching, pair Gemini with an Ollama embedding model via `[llm.orchestrator]`.

## Features

All backends (Ollama, Claude, OpenAI, Gemini, compatible) and the OpenAI Whisper STT path are always compiled — no default features are required. `schemars`, `chat_typed`, and `Extractor` are always available. The optional features below add local inference and specialized backends.

| Feature | Default | Description |
|---------|---------|-------------|
| `candle` | off | Local inference via Candle; pulls in `candle-core`, `candle-nn`, `candle-transformers`, `hf-hub`, `tokenizers`, and the audio-decode/resample stack used by local Whisper (`symphonia`, `rubato` 5.x, `audioadapter-buffers` 5.x). Enables `candle_provider` and `candle_whisper` |
| `classifiers` | off | Candle-backed classifiers (implies `candle`) |
| `cuda` | off | CUDA backend for Candle (implies `candle`) |
| `metal` | off | Metal backend for Candle on Apple Silicon (implies `candle`) |
| `gonka` | off | Gonka native signed-transport backend (`k256`, `bech32`, `ripemd`) |
| `cocoon` | off | Cocoon provider integration |
| `testing` | off | Exposes `MockProvider` for unit testing without a live LLM endpoint |
| `profiling` | off | Extra tracing spans for latency profiling |

## Installation

```bash
cargo add zeph-llm

# With local inference via Candle
cargo add zeph-llm --features candle

# With the Gonka native backend
cargo add zeph-llm --features gonka
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
