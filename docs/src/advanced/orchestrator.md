# Model Orchestrator

Route tasks to different LLM providers based on content classification. Each task type maps to a provider chain with automatic fallback. Use the orchestrator to combine local and cloud models — for example, embeddings via Ollama and chat via Claude.

## Configuration

```toml
[llm]
provider = "orchestrator"

[llm.orchestrator]
default = "claude"
embed = "ollama"

[llm.orchestrator.providers.ollama]
type = "ollama"

[llm.orchestrator.providers.claude]
type = "claude"

[llm.orchestrator.routes]
coding = ["claude", "ollama"]       # try Claude first, fallback to Ollama
creative = ["claude"]               # cloud only
analysis = ["claude", "ollama"]     # prefer cloud
general = ["claude"]                # cloud only
```

## Sub-Provider Fields

Each entry under `[llm.orchestrator.providers.<name>]` supports:

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Provider backend: `ollama`, `claude`, `openai`, `candle`, `compatible` |
| `model` | string? | Override chat model |
| `base_url` | string? | Override API endpoint (Ollama / Compatible) |
| `embedding_model` | string? | Override embedding model |
| `filename` | string? | GGUF filename (Candle only) |
| `device` | string? | Compute device: `cpu`, `metal`, `cuda` (Candle only) |

## Fallback Chain for Provider Fields

Per-sub-provider fields override parent config. Resolution order:

1. **Per-provider field** — e.g. `[llm.orchestrator.providers.ollama].base_url`
2. **Parent section** — e.g. `[llm].base_url` for Ollama, `[llm.cloud].model` for Claude
3. **Global default** — compiled-in defaults

This allows a single `[llm]` section to set shared defaults while individual sub-providers override only what differs.

## Provider Keys

- `default` — provider for chat when no specific route matches
- `embed` — provider for all embedding operations (skill matching, semantic memory)

## Task Classification

Task types are classified via keyword heuristics:

| Task Type | Keywords |
|-----------|----------|
| `coding` | code, function, debug, refactor, implement |
| `creative` | write, story, poem, creative |
| `analysis` | analyze, compare, evaluate |
| `translation` | translate, convert language |
| `summarization` | summarize, summary, tldr |
| `general` | everything else |

## Fallback Chains

Routes define provider preference order. If the first provider fails, the next one in the list is tried automatically.

```toml
coding = ["local", "cloud"]  # try local first, fallback to cloud
```

## Interactive Setup

Run `zeph init` and select **Orchestrator** as the LLM provider. The wizard prompts for:

1. **Primary provider** — select from Ollama, Claude, OpenAI, or Compatible. Provide the model name, base URL, and API key as needed.
2. **Fallback provider** — same selection. The fallback activates when the primary fails.
3. **Embedding model** — used for skill matching and semantic memory.

The wizard generates a complete `[llm.orchestrator]` section with provider map, `chat` route (primary + fallback), and `embed` route.

## Multi-Instance Example

Two Ollama servers on different ports — one for chat, one for embeddings:

```toml
[llm]
provider = "orchestrator"
base_url = "http://localhost:11434"
embedding_model = "qwen3-embedding"

[llm.orchestrator]
default = "ollama-chat"
embed = "ollama-embed"

[llm.orchestrator.providers.ollama-chat]
type = "ollama"
model = "mistral:7b"
# inherits base_url from [llm].base_url

[llm.orchestrator.providers.ollama-embed]
type = "ollama"
base_url = "http://localhost:11435"       # second Ollama instance
embedding_model = "nomic-embed-text"      # dedicated embedding model

[llm.orchestrator.routes]
general = ["ollama-chat"]
```

## Hybrid Setup Example

Embeddings via free local Ollama, chat via paid Claude API:

```toml
[llm]
provider = "orchestrator"

[llm.orchestrator]
default = "claude"
embed = "ollama"

[llm.orchestrator.providers.ollama]
type = "ollama"

[llm.orchestrator.providers.claude]
type = "claude"

[llm.orchestrator.routes]
general = ["claude"]
```
