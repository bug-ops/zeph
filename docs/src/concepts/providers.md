# LLM Providers

Zeph supports multiple LLM backends. Choose based on your needs:

| Provider | Type | Embeddings | Vision | Best For |
|----------|------|-----------|--------|----------|
| Ollama | Local | Yes | Yes | Privacy, free, offline |
| Claude | Cloud | No | Yes | Quality, reasoning |
| OpenAI | Cloud | Yes | Yes | Ecosystem, compatibility |
| Compatible | Cloud | Varies | Varies | Together AI, Groq, Fireworks |
| Candle | Local | No | No | Minimal footprint |

Claude does not support embeddings natively. Use the [orchestrator](../advanced/orchestrator.md) to combine Claude chat with Ollama embeddings.

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

## Switching Providers

One config change: set `provider` in `[llm]`. All skills, memory, and tools work the same regardless of which provider is active.

```toml
[llm]
provider = "claude"   # ollama, claude, openai, candle, compatible, orchestrator, router
```

Or via environment variable: `ZEPH_LLM_PROVIDER`.

## Deep Dives

- [Use a Cloud Provider](../guides/cloud-provider.md) — Claude, OpenAI, and compatible API setup
- [Model Orchestrator](../advanced/orchestrator.md) — multi-provider routing with fallback chains
- [Local Inference (Candle)](../advanced/candle.md) — HuggingFace GGUF models
