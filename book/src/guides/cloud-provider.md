# Use a Cloud Provider

Connect Zeph to Claude, OpenAI, Gemini, or any OpenAI-compatible API instead of local Ollama.

> **Breaking change (v0.17.0):** The old `[llm.cloud]`, `[llm.orchestrator]`, and `[llm.router]` config sections have been removed. Run `zeph --migrate-config` to automatically convert your config file.

## Claude

```bash
ZEPH_CLAUDE_API_KEY=sk-ant-... zeph
```

Or in config:

```toml
[llm]
[[llm.providers]]
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
# server_compaction = true          # Server-side context compaction (Claude API beta)
# enable_extended_context = true    # 1M token context window (Sonnet/Opus 4.6 only)
```

Claude does not support embeddings. Use a multi-provider setup to combine Claude chat with Ollama embeddings, or use OpenAI embeddings.

### Server-Side Compaction

Enable `server_compaction = true` to let the Claude API manage context length on the server side. When the context approaches the model's limit, Claude produces a compact summary in-place. Zeph surfaces the compaction event in the TUI and via the `server_compaction_events` metric.

> **Note:** Server compaction is not supported on Haiku models. When enabled on Haiku, Zeph emits a `WARN` and falls back to client-side compaction automatically.

### 1M Extended Context

For Sonnet 4.6 and Opus 4.6, enable `enable_extended_context = true` to unlock the 1M token context window. The `auto_budget` feature scales accordingly. Enable with `--extended-context` CLI flag or in the provider entry in config.

## Gemini

```bash
ZEPH_GEMINI_API_KEY=AIza... zeph
```

Or in config:

```toml
[llm]
[[llm.providers]]
type = "gemini"
model = "gemini-2.0-flash"    # or "gemini-2.5-pro" for extended thinking
max_tokens = 8192
# embedding_model = "text-embedding-004"  # enable Gemini-native embeddings
# thinking_level = "medium"              # Gemini 2.5+ only: minimal, low, medium, high
```

Gemini supports embeddings natively when `embedding_model` is set — no separate Ollama instance required. See [LLM Providers — Gemini](../concepts/providers.md#gemini) for the full feature matrix.

## OpenAI

```bash
ZEPH_OPENAI_API_KEY=sk-... zeph
```

```toml
[llm]
[[llm.providers]]
type = "openai"
base_url = "https://api.openai.com/v1"
model = "gpt-5.2"
max_tokens = 4096
embedding_model = "text-embedding-3-small"
reasoning_effort = "medium"   # optional: low, medium, high (for o3, etc.)
```

When `embedding_model` is set, Qdrant subsystems use it automatically for skill matching and semantic memory.

## Compatible APIs

Use `type = "compatible"` with the appropriate `base_url`:

```toml
[llm]
[[llm.providers]]
name = "groq"
type = "compatible"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.3-70b-versatile"
max_tokens = 4096
```

Common `base_url` values:

| Provider | `base_url` |
|---|---|
| Together AI | `https://api.together.xyz/v1` |
| Groq | `https://api.groq.com/openai/v1` |
| Fireworks | `https://api.fireworks.ai/inference/v1` |
| Local vLLM | `http://localhost:8000/v1` |

## Hybrid Setup

Embeddings via free local Ollama, chat via paid Claude API:

```toml
[llm]
routing = "cascade"   # try cheapest provider first

[[llm.providers]]
name = "local"
type = "ollama"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"
embed = true          # use this provider for embeddings

[[llm.providers]]
name = "cloud"
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
default = true        # use this provider for chat by default
```

See [Adaptive Inference](../advanced/adaptive-inference.md) for routing strategy options.

## Interactive Setup

Run `zeph init` and select your provider in Step 2. The wizard handles model names, base URLs, and API keys. See [Configuration Wizard](../getting-started/wizard.md).
