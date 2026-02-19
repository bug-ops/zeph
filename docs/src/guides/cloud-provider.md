# Use a Cloud Provider

Connect Zeph to Claude, OpenAI, or any OpenAI-compatible API instead of local Ollama.

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
```

Claude does not support embeddings. Use the [orchestrator](../advanced/orchestrator.md) to combine Claude chat with Ollama embeddings, or use OpenAI embeddings.

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
