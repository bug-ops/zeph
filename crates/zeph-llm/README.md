# zeph-llm

[![Crates.io](https://img.shields.io/crates/v/zeph-llm)](https://crates.io/crates/zeph-llm)
[![docs.rs](https://img.shields.io/docsrs/zeph-llm)](https://docs.rs/zeph-llm)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

LLM provider abstraction with Ollama, Claude, OpenAI, Gemini, and Candle backends.

## Overview

Defines the `LlmProvider` trait and ships concrete backends for Ollama, Claude, OpenAI, Google Gemini, and OpenAI-compatible endpoints. Includes an orchestrator for multi-model coordination, a router for model selection, an optional Candle backend for local inference, and an SQLite-backed response cache with blake3 key hashing and TTL expiry.

## Key modules

| Module | Description |
|--------|-------------|
| `provider` | `LlmProvider` trait — unified inference interface; `name()` returns `&str` (no longer `&'static str`); `Message` carries `MessageMetadata` with `agent_visible`/`user_visible` flags for dual-visibility control |
| `ollama` | Ollama HTTP backend |
| `claude` | Anthropic Claude backend with `with_client()` builder for shared `reqwest::Client` |
| `openai` | OpenAI backend with `with_client()` builder for shared `reqwest::Client` |
| `gemini` | Google Gemini backend (`generateContent` + `streamGenerateContent?alt=sse`); system prompt mapped to `systemInstruction`, `assistant` role to `"model"`, consecutive same-role message merging, thinking parts surfaced as `StreamChunk::Thinking`, `functionCall` parts in SSE stream emitted as `StreamChunk::ToolUse`; configured via `[llm.gemini]` and `ZEPH_GEMINI_API_KEY` |
| `compatible` | Generic OpenAI-compatible endpoint backend |
| `candle_provider` | Local inference via Candle (optional feature) |
| `orchestrator` | Multi-model coordination and fallback; `send_with_retry()` helper deduplicates retry logic |
| `router` | Model selection and routing logic with two strategies: EMA latency tracking and Thompson Sampling (Beta distributions). `RouterProvider` dispatches to the configured strategy and records outcomes per provider. Providers stored as `Arc<[AnyProvider]>` — `clone()` on every LLM request is O(1) regardless of chain length |
| `vision` | Image input support — base64-encoded images in LLM requests; optional dedicated `vision_model` per provider |
| `extractor` | `chat_typed<T>()` — typed LLM output via JSON Schema (`schemars`); per-`TypeId` schema caching |
| `sse` | Shared `sse_to_chat_stream()` helpers for Claude and OpenAI SSE parsing |
| `stt` | `SpeechToText` trait and `WhisperProvider` (OpenAI Whisper, feature-gated behind `stt`) |
| `candle_whisper` | Local offline STT via Candle (whisper-tiny/base/small, feature-gated behind `candle`) |
| `http` | `default_client()` — shared HTTP client with standard timeouts and user-agent |
| `error` | `LlmError` — unified error type; `ContextLengthExceeded` variant with `is_context_length_error()` heuristic matching across provider error formats (Claude, OpenAI, Ollama) |

**Re-exports:** `LlmProvider`, `LlmError`

## Router strategies

The router supports two strategies for ordering providers in the fallback chain. Set the strategy in `[llm.router]`:

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

## SLM provider recommendations

For cost-sensitive or resource-constrained deployments, the following Small Language Models are verified to work well with Zeph:

| Task | Recommended SLM | Notes |
|------|----------------|-------|
| Embeddings | `nomic-embed-text` (Ollama) | Default embedding model |
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
| `Extended { budget_tokens }` | Allocates a fixed token budget (1024–128000) for visible reasoning; emits `interleaved-thinking-2025-05-14` beta header on Sonnet 4.6 with tools |
| `Adaptive { effort? }` | Lets the model allocate thinking budget automatically |

```toml
[llm.claude]
thinking = { mode = "extended", budget_tokens = 16000 }
```

CLI: `--thinking extended:16000` or `--thinking adaptive`. When thinking is enabled and `max_tokens` is below 16000, it is raised automatically. Thinking deltas are parsed from the SSE stream and suppressed from the user-facing output; `MessagePart::ThinkingBlock` variants preserve thinking blocks verbatim across tool-use turns.

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

| Feature | Default | Description |
|---------|---------|-------------|
| `schema` | on | `schemars` dependency, `chat_typed`, `Extractor`, and per-`TypeId` schema caching |
| `mock` | off | `MockProvider` for unit testing without a live LLM endpoint |
| `stt` | off | `WhisperProvider` using OpenAI Whisper API (requires `reqwest/multipart`) |
| `candle` | off | Local GGUF inference via Candle; pulls in `candle-core`, `candle-nn`, `candle-transformers`, `hf-hub`, `tokenizers`, `symphonia`, `rubato` |
| `cuda` | off | Enables CUDA backend for Candle (implies `candle`) |
| `metal` | off | Enables Metal backend for Candle on Apple Silicon (implies `candle`) |

To compile without `schemars`:

```bash
cargo build -p zeph-llm --no-default-features
```

## Installation

```bash
cargo add zeph-llm

# Without schemars (chat_typed and Extractor not available)
cargo add zeph-llm --no-default-features

# With local inference via Candle
cargo add zeph-llm --features candle

# With OpenAI Whisper STT
cargo add zeph-llm --features stt
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
