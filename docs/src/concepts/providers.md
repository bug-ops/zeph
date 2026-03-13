# LLM Providers

Zeph supports multiple LLM backends. Choose based on your needs:

| Provider | Type | Embeddings | Vision | Streaming | Best For |
|----------|------|-----------|--------|-----------|----------|
| Ollama | Local | Yes | Yes | Yes | Privacy, free, offline |
| Claude | Cloud | No | Yes | Yes | Quality, reasoning, prompt caching |
| OpenAI | Cloud | Yes | Yes | Yes | Ecosystem, GPT-4o, GPT-5 |
| Gemini | Cloud | No | Yes | Yes | Google ecosystem, long context |
| Compatible | Cloud | Varies | Varies | Varies | Together AI, Groq, Fireworks |
| Candle | Local | No | No | No | Minimal footprint |

Claude and Gemini do not support embeddings natively. Use the [orchestrator](../advanced/orchestrator.md) to combine them with Ollama embeddings.

## Quick Setup

**Ollama** (default — no API key needed):

```bash
ollama pull mistral:7b
ollama pull qwen3-embedding
zeph
```

**Claude**:

```bash
ZEPH_CLAUDE_API_KEY=sk-ant-... zeph
```

**OpenAI**:

```bash
ZEPH_LLM_PROVIDER=openai ZEPH_OPENAI_API_KEY=sk-... zeph
```

**Gemini**:

```bash
ZEPH_LLM_PROVIDER=gemini ZEPH_GEMINI_API_KEY=AIza... zeph
```

## Gemini

Zeph supports Google Gemini as a first-class LLM backend. Gemini is a strong choice when you want access to Google's latest models (Gemini 2.5 Pro, Gemini 2.0 Flash), very long context windows, or native multimodal reasoning. It pairs well with Ollama embeddings via the [orchestrator](../advanced/orchestrator.md) since it does not provide an embedding API.

### Why Gemini

Google's Gemini 2.5 family brings extended thinking (visible as streaming `Thinking` chunks in Zeph's TUI) and multi-million-token context windows. For tasks that require deep reasoning over large codebases or long documents, Gemini's context capacity complements Zeph's existing RAG pipeline.

### Integration Overview

The `GeminiProvider` translates Zeph's internal message format to Gemini's `generateContent` API:

- The system prompt becomes a top-level `systemInstruction` field (Gemini's required format).
- The `assistant` role is mapped to `"model"` (Gemini's terminology for the model turn).
- Consecutive messages with the same role are automatically merged — Gemini requires strict user/model alternation.
- If the conversation starts with a model turn, a synthetic empty user message is prepended to satisfy the API contract.

Streaming uses `streamGenerateContent?alt=sse`. Thinking parts (returned with `thought: true` by Gemini 2.5 models) are surfaced as `StreamChunk::Thinking` and shown in the TUI sidebar.

### Configuration

```toml
[llm]
provider = "gemini"

[llm.gemini]
model = "gemini-2.0-flash"           # default; use "gemini-2.5-pro" for extended thinking
max_tokens = 8192
# base_url = "https://generativelanguage.googleapis.com/v1beta"  # default
```

Store the API key in the vault (recommended):

```bash
zeph vault set ZEPH_GEMINI_API_KEY AIza...
```

Or export it as an environment variable:

```bash
export ZEPH_GEMINI_API_KEY=AIza...
```

Run `zeph init` and choose Gemini as the provider to have the wizard generate a complete config with all Gemini parameters.

### Streaming and Thinking

When streaming is active, Zeph emits chunks as they arrive from the SSE stream. For Gemini 2.5 models that return thinking parts, the TUI shows a "Thinking…" indicator while the model reasons and then switches to the response stream. Both paths use the same retry infrastructure (`send_with_retry`) — HTTP 429 (rate limit) and 503 (service unavailable) responses trigger automatic backoff and retry.

### Limitations

- Gemini does not expose an embeddings endpoint compatible with Zeph's `embed()` interface. Combine with Ollama (via `[llm.orchestrator]`) for semantic memory and skill matching.
- Function calling support is planned but not yet available in the Gemini provider.

## Switching Providers

One config change: set `provider` in `[llm]`. All skills, memory, and tools work the same regardless of which provider is active.

```toml
[llm]
provider = "claude"   # ollama, claude, openai, gemini, candle, compatible, orchestrator, router
```

Or via environment variable: `ZEPH_LLM_PROVIDER`.

## Response Caching

Enable SQLite-backed response caching to avoid redundant LLM calls for identical requests. The cache key is a blake3 hash of the full message history and model name. Streaming responses bypass the cache.

```toml
[llm]
response_cache_enabled = true
response_cache_ttl_secs = 3600  # 1 hour (default)
```

See [Memory and Context — LLM Response Cache](memory.md#llm-response-cache) for details.

## Deep Dives

- [Use a Cloud Provider](../guides/cloud-provider.md) — Claude, OpenAI, and compatible API setup
- [Model Orchestrator](../advanced/orchestrator.md) — multi-provider routing with fallback chains
- [Adaptive Inference](../advanced/adaptive-inference.md) — Thompson Sampling and EMA-based provider routing
- [Local Inference (Candle)](../advanced/candle.md) — HuggingFace GGUF models
