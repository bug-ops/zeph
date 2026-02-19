# zeph-llm

LLM provider abstraction and backend implementations.

## Overview

Defines the `LlmProvider` trait and ships concrete backends for Ollama, Claude, OpenAI, and OpenAI-compatible endpoints. Includes an orchestrator for multi-model coordination, a router for model selection, and an optional Candle backend for local inference.

## Key modules

| Module | Description |
|--------|-------------|
| `provider` | `LlmProvider` trait — unified inference interface |
| `ollama` | Ollama HTTP backend |
| `claude` | Anthropic Claude backend |
| `openai` | OpenAI backend |
| `compatible` | Generic OpenAI-compatible endpoint backend |
| `candle_provider` | Local inference via Candle (optional feature) |
| `orchestrator` | Multi-model coordination and fallback |
| `router` | Model selection and routing logic |
| `vision` | Image input support — base64-encoded images in LLM requests; optional dedicated `vision_model` per provider |
| `stt` | `SpeechToText` trait and `WhisperProvider` (OpenAI Whisper, feature-gated behind `stt`) |
| `candle_whisper` | Local offline STT via Candle (whisper-tiny/base/small, feature-gated behind `candle`) |
| `error` | `LlmError` — unified error type |

**Re-exports:** `LlmProvider`, `LlmError`

## Usage

```toml
[dependencies]
zeph-llm = { path = "../zeph-llm" }
```

## License

MIT
