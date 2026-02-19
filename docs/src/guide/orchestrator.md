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
provider_type = "ollama"

[llm.orchestrator.providers.claude]
provider_type = "claude"

[llm.orchestrator.routes]
coding = ["claude", "ollama"]       # try Claude first, fallback to Ollama
creative = ["claude"]               # cloud only
analysis = ["claude", "ollama"]     # prefer cloud
general = ["claude"]                # cloud only
```

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

## Hybrid Setup Example

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
