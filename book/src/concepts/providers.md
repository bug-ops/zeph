# LLM Providers

Zeph supports multiple LLM backends. Choose based on your needs:

| Provider | Type | Embeddings | Vision | Streaming | Best For |
|----------|------|-----------|--------|-----------|----------|
| Ollama | Local | Yes | Yes | Yes | Privacy, free, offline |
| Claude | Cloud | No | Yes | Yes | Quality, reasoning, prompt caching |
| OpenAI | Cloud | Yes | Yes | Yes | Ecosystem, GPT-4o, GPT-5 |
| Gemini | Cloud | Yes | Yes | Yes | Google ecosystem, long context, extended thinking |
| Compatible | Cloud | Varies | Varies | Varies | Together AI, Groq, Fireworks |
| Candle | Local | No | No | No | Minimal footprint |

Claude does not support embeddings natively. Use a multi-provider setup with `embed = true` on an Ollama or OpenAI provider entry to combine Claude chat with local embeddings. Gemini supports embeddings via the `text-embedding-004` model — set `embedding_model` in the Gemini `[[llm.providers]]` entry to enable.

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

Zeph supports Google Gemini as a first-class LLM backend. Gemini is a strong choice when you want access to Google's latest models (Gemini 2.5 Pro, Gemini 2.0 Flash), very long context windows, extended thinking, or native multimodal reasoning.

### Why Gemini

Google's Gemini 2.5 family brings extended thinking (visible as streaming `Thinking` chunks in Zeph's TUI), native tool use, vision, and embeddings. For tasks that require deep reasoning over large codebases or long documents, Gemini's context capacity complements Zeph's existing RAG pipeline.

### Integration Overview

The `GeminiProvider` translates Zeph's internal message format to Gemini's `generateContent` API:

- The system prompt becomes a top-level `systemInstruction` field (Gemini's required format).
- The `assistant` role is mapped to `"model"` (Gemini's terminology for the model turn).
- Consecutive messages with the same role are automatically merged — Gemini requires strict user/model alternation.
- If the conversation starts with a model turn, a synthetic empty user message is prepended to satisfy the API contract.
- Tool definitions are converted to Gemini `functionDeclarations` with JSON schema normalization (`$ref` inlining, `anyOf`/`oneOf` → `nullable`, type name uppercasing).
- Vision inputs are sent as `inlineData` parts with base64-encoded image data.

Streaming uses `streamGenerateContent?alt=sse`. Thinking parts (returned with `thought: true` by Gemini 2.5 models) are surfaced as `StreamChunk::Thinking` and shown in the TUI sidebar.

### Configuration

```toml
[llm]
[[llm.providers]]
type = "gemini"
model = "gemini-2.0-flash"           # default; use "gemini-2.5-pro" for extended thinking
max_tokens = 8192
# embedding_model = "text-embedding-004"  # enable Gemini embeddings (optional)
# thinking_level = "medium"              # minimal, low, medium, high (Gemini 2.5+)
# thinking_budget = 8192                 # token budget for thinking; -1 = dynamic, 0 = off
# include_thoughts = true                # surface thinking chunks in TUI
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

Run `zeph init` and choose Gemini as the provider to have the wizard generate a complete config with all Gemini parameters, including the thinking level prompt.

### Capabilities

| Feature | Gemini 2.0 Flash | Gemini 2.5 Pro |
|---------|-----------------|----------------|
| Chat | Yes | Yes |
| Streaming (SSE) | Yes | Yes |
| Tool use | Yes | Yes |
| Streaming tool use | Yes | Yes |
| Vision | Yes | Yes |
| Embeddings | Yes (`text-embedding-004`) | Yes (`text-embedding-004`) |
| Extended thinking | No | Yes (`thinking_level` / `thinking_budget`) |
| Remote model discovery | Yes | Yes |

### Embeddings

Set `embedding_model` in the Gemini `[[llm.providers]]` entry to enable Gemini embeddings. When set, `supports_embeddings()` returns `true` and Zeph uses `POST /v1beta/models/{model}:embedContent` for semantic memory and skill matching — no Ollama dependency required.

```toml
[[llm.providers]]
type = "gemini"
model = "gemini-2.0-flash"
embedding_model = "text-embedding-004"
```

### Streaming and Thinking

When streaming is active, Zeph emits chunks as they arrive from the SSE stream (`streamGenerateContent?alt=sse`). For Gemini 2.5 models that return thinking parts, the TUI shows a "Thinking…" indicator while the model reasons and then switches to the response stream. Both paths use the same retry infrastructure (`send_with_retry`) — HTTP 429 (rate limit) and 503 (service unavailable) responses trigger automatic backoff and retry.

Configure thinking via `thinking_level` (categorical) or `thinking_budget` (token count). Both fields are optional and apply only to Gemini 2.5+ models.

### Streaming Tool Use

Gemini delivers `functionCall` parts as complete objects within a single SSE event (not incrementally chunked). The SSE parser collects all `functionCall` parts from the event's `parts` array and emits a single `StreamChunk::ToolUse` with all tool calls. When an event contains both text and function call parts, tool calls take priority and any text in that event is dropped (matching the non-streaming behavior).

Streaming tool use is available on all Gemini models that support function calling, including Gemini 2.0 Flash.

## Switching Providers

Change the `type` field in the `[[llm.providers]]` entry. All skills, memory, and tools work the same regardless of which provider is active.

```toml
[llm]
[[llm.providers]]
type = "claude"   # ollama, claude, openai, gemini, candle, compatible
model = "claude-sonnet-4-6"
```

## Response Caching

Enable SQLite-backed response caching to avoid redundant LLM calls for identical requests. The cache key is a blake3 hash of the full message history and model name. Streaming responses bypass the cache.

```toml
[llm]
response_cache_enabled = true
response_cache_ttl_secs = 3600  # 1 hour (default)
```

See [Memory and Context — LLM Response Cache](memory.md#llm-response-cache) for details.

## Next Steps

- [Use a Cloud Provider](../guides/cloud-provider.md) — Claude, OpenAI, and compatible API setup
- [Model Orchestrator](../advanced/orchestrator.md) — multi-provider routing with fallback chains
- [Adaptive Inference](../advanced/adaptive-inference.md) — Thompson Sampling and EMA-based provider routing
- [SkillOrchestra](../advanced/skill-orchestra.md) — RL-based adaptive routing that learns from execution outcomes
- [Local Inference (Candle)](../advanced/candle.md) — HuggingFace GGUF models
