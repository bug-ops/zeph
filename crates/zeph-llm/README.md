# zeph-llm

[![Crates.io](https://img.shields.io/crates/v/zeph-llm)](https://crates.io/crates/zeph-llm)
[![docs.rs](https://img.shields.io/docsrs/zeph-llm)](https://docs.rs/zeph-llm)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

LLM provider abstraction with Ollama, Claude, OpenAI, and Candle backends.

## Overview

Defines the `LlmProvider` trait and ships concrete backends for Ollama, Claude, OpenAI, and OpenAI-compatible endpoints. Includes an orchestrator for multi-model coordination, a router for model selection, an optional Candle backend for local inference, and an SQLite-backed response cache with blake3 key hashing and TTL expiry.

## Key modules

| Module | Description |
|--------|-------------|
| `provider` | `LlmProvider` trait — unified inference interface; `name()` returns `&str` (no longer `&'static str`); `Message` carries `MessageMetadata` with `agent_visible`/`user_visible` flags for dual-visibility control |
| `ollama` | Ollama HTTP backend |
| `claude` | Anthropic Claude backend with `with_client()` builder for shared `reqwest::Client` |
| `openai` | OpenAI backend with `with_client()` builder for shared `reqwest::Client` |
| `compatible` | Generic OpenAI-compatible endpoint backend |
| `candle_provider` | Local inference via Candle (optional feature) |
| `orchestrator` | Multi-model coordination and fallback; `send_with_retry()` helper deduplicates retry logic |
| `router` | Model selection and routing logic; `EmaTracker` maintains per-provider exponential moving average latency; when `router_ema_enabled = true`, providers are periodically reordered by EMA score |
| `vision` | Image input support — base64-encoded images in LLM requests; optional dedicated `vision_model` per provider |
| `extractor` | `chat_typed<T>()` — typed LLM output via JSON Schema (`schemars`); per-`TypeId` schema caching |
| `sse` | Shared `sse_to_chat_stream()` helpers for Claude and OpenAI SSE parsing |
| `stt` | `SpeechToText` trait and `WhisperProvider` (OpenAI Whisper, feature-gated behind `stt`) |
| `candle_whisper` | Local offline STT via Candle (whisper-tiny/base/small, feature-gated behind `candle`) |
| `http` | `default_client()` — shared HTTP client with standard timeouts and user-agent |
| `error` | `LlmError` — unified error type; `ContextLengthExceeded` variant with `is_context_length_error()` heuristic matching across provider error formats (Claude, OpenAI, Ollama) |

**Re-exports:** `LlmProvider`, `LlmError`

## EMA routing

When `router_ema_enabled = true`, `EmaTracker` records per-provider call latency after each successful response. Providers are reordered by EMA score every `router_reorder_interval` seconds, so the fastest reliable provider is tried first.

```toml
[llm]
router_ema_enabled      = true
router_ema_alpha        = 0.1   # smoothing factor; lower = slower to adapt
router_reorder_interval = 60    # seconds between reordering
```

> [!NOTE]
> EMA routing is disabled by default. It is most useful in multi-provider setups where provider latencies differ significantly or change over time.

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
```

## License

MIT
