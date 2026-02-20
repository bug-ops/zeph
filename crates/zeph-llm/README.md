# zeph-llm

LLM provider abstraction and backend implementations.

## Overview

Defines the `LlmProvider` trait and ships concrete backends for Ollama, Claude, OpenAI, and OpenAI-compatible endpoints. Includes an orchestrator for multi-model coordination, a router for model selection, and an optional Candle backend for local inference.

## Key modules

| Module | Description |
|--------|-------------|
| `provider` | `LlmProvider` trait — unified inference interface; `name()` returns `&str` (no longer `&'static str`) |
| `ollama` | Ollama HTTP backend |
| `claude` | Anthropic Claude backend with `with_client()` builder for shared `reqwest::Client` |
| `openai` | OpenAI backend with `with_client()` builder for shared `reqwest::Client` |
| `compatible` | Generic OpenAI-compatible endpoint backend |
| `candle_provider` | Local inference via Candle (optional feature) |
| `orchestrator` | Multi-model coordination and fallback; `send_with_retry()` helper deduplicates retry logic |
| `router` | Model selection and routing logic |
| `vision` | Image input support — base64-encoded images in LLM requests; optional dedicated `vision_model` per provider |
| `extractor` | `chat_typed<T>()` — typed LLM output via JSON Schema (`schemars`); per-`TypeId` schema caching |
| `sse` | Shared `sse_to_chat_stream()` helpers for Claude and OpenAI SSE parsing |
| `stt` | `SpeechToText` trait and `WhisperProvider` (OpenAI Whisper, feature-gated behind `stt`) |
| `candle_whisper` | Local offline STT via Candle (whisper-tiny/base/small, feature-gated behind `candle`) |
| `http` | `default_client()` — shared HTTP client with standard timeouts and user-agent |
| `error` | `LlmError` — unified error type |

**Re-exports:** `LlmProvider`, `LlmError`

## Usage

```toml
[dependencies]
zeph-llm = { path = "../zeph-llm" }
```

## License

MIT
