# Use a Cloud Provider

Connect Zeph to Claude, OpenAI, Gemini, or any OpenAI-compatible API instead of local Ollama.

## Claude

```bash
ZEPH_CLAUDE_API_KEY=sk-ant-... zeph
```

Or in config:

```toml
[llm]
provider = "claude"

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 4096
# server_compaction = true          # Server-side context compaction (Claude API beta)
# enable_extended_context = true    # 1M token context window (Sonnet/Opus 4.6 only)
```

Claude does not support embeddings. Use the [orchestrator](../advanced/orchestrator.md) to combine Claude chat with Ollama embeddings, or use OpenAI embeddings.

### Server-Side Compaction

Enable `server_compaction = true` to let the Claude API manage context length on the server side. When the context approaches the model's limit, Claude produces a compact summary in-place. Zeph surfaces the compaction event in the TUI and via the `server_compaction_events` metric.

> **Note:** Server compaction is not supported on Haiku models. When enabled on Haiku, Zeph emits a `WARN` and falls back to client-side compaction automatically.

### 1M Extended Context

For Sonnet 4.6 and Opus 4.6, enable `enable_extended_context = true` to unlock the 1M token context window. The `auto_budget` feature scales accordingly. Enable with `--server-compaction` CLI flag or in `[llm.cloud]` in config.

## Gemini

```bash
ZEPH_LLM_PROVIDER=gemini ZEPH_GEMINI_API_KEY=AIza... zeph
```

Or in config:

```toml
[llm]
provider = "gemini"

[llm.gemini]
model = "gemini-2.0-flash"    # or "gemini-2.5-pro" for extended thinking
max_tokens = 8192
# embedding_model = "text-embedding-004"  # enable Gemini-native embeddings
# thinking_level = "medium"              # Gemini 2.5+ only: minimal, low, medium, high
```

Gemini supports embeddings natively when `embedding_model` is set — no separate Ollama instance required. See [LLM Providers — Gemini](../concepts/providers.md#gemini) for the full feature matrix.

## OpenAI

```bash
ZEPH_LLM_PROVIDER=openai ZEPH_OPENAI_API_KEY=sk-... zeph
```

```toml
[llm]
provider = "openai"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-5.2"
max_tokens = 4096
embedding_model = "text-embedding-3-small"
reasoning_effort = "medium"   # optional: low, medium, high (for o3, etc.)
```

When `embedding_model` is set, Qdrant subsystems use it automatically for skill matching and semantic memory.

## Compatible APIs

Change `base_url` to point to any OpenAI-compatible endpoint:

```toml
# Together AI
base_url = "https://api.together.xyz/v1"

# Groq
base_url = "https://api.groq.com/openai/v1"

# Fireworks
base_url = "https://api.fireworks.ai/inference/v1"
```

## Hybrid Setup

Embeddings via free local Ollama, chat via paid Claude API:

```toml
[llm]
provider = "orchestrator"

[llm.orchestrator]
default = "claude"
embed = "ollama"

[llm.orchestrator.providers.ollama]
provider_type = "ollama"

[llm.orchestrator.providers.claude]
provider_type = "claude"

[llm.orchestrator.routes]
general = ["claude"]
```

See [Model Orchestrator](../advanced/orchestrator.md) for task classification and fallback chain options.

## Interactive Setup

Run `zeph init` and select your provider in Step 2. The wizard handles model names, base URLs, and API keys. See [Configuration Wizard](../getting-started/wizard.md).
