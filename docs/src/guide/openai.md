# OpenAI Provider

Connect to OpenAI API or any OpenAI-compatible service (Together AI, Groq, Fireworks, Perplexity).

```bash
ZEPH_LLM_PROVIDER=openai ZEPH_OPENAI_API_KEY=sk-... ./target/release/zeph
```

## Configuration

```toml
[llm]
provider = "openai"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-5.2"
max_tokens = 4096
embedding_model = "text-embedding-3-small"   # optional, enables vector embeddings
reasoning_effort = "medium"                  # optional: low, medium, high (for o3, etc.)
```

## Compatible APIs

Change `base_url` to point to any OpenAI-compatible API:

```toml
# Together AI
base_url = "https://api.together.xyz/v1"

# Groq
base_url = "https://api.groq.com/openai/v1"

# Fireworks
base_url = "https://api.fireworks.ai/inference/v1"
```

## Embeddings

When `embedding_model` is set, Qdrant subsystems automatically use it for skill matching and semantic memory instead of the global `llm.embedding_model`.

## Reasoning Models

Set `reasoning_effort` to control token budget for reasoning models like `o3`:

- `low` — fast responses, less reasoning
- `medium` — balanced
- `high` — thorough reasoning, more tokens
