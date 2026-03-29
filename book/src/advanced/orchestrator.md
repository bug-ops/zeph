# Model Orchestrator

> **Tip:** For simple fallback chains with adaptive routing (Thompson Sampling or EMA), use `routing = "cascade"` or `routing = "thompson"` in `[llm]` instead. See [Adaptive Inference](adaptive-inference.md).

Route tasks to different LLM providers based on content classification. Each task type maps to a provider chain with automatic fallback. Use a multi-provider setup to combine local and cloud models — for example, embeddings via Ollama and chat via Claude.

## Configuration

```toml
[llm]
routing = "task"   # task-based routing

[[llm.providers]]
name = "ollama"
type = "ollama"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"
embed = true        # use this provider for all embedding operations

[[llm.providers]]
name = "claude"
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
default = true      # default provider for chat
```

## Provider Entry Fields

Each `[[llm.providers]]` entry supports:

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Provider backend: `ollama`, `claude`, `openai`, `gemini`, `candle`, `compatible` |
| `name` | string? | Identifier for routing; required for `type = "compatible"` |
| `model` | string? | Chat model |
| `base_url` | string? | API endpoint (Ollama / Compatible) |
| `embedding_model` | string? | Embedding model |
| `embed` | bool | Mark as the embedding provider for skill matching and semantic memory |
| `default` | bool | Mark as the primary chat provider |
| `filename` | string? | GGUF filename (Candle only) |
| `device` | string? | Compute device: `cpu`, `metal`, `cuda` (Candle only) |

## Provider Selection

- `default = true` — provider used for chat when no other routing rule matches
- `embed = true` — provider used for all embedding operations (skill matching, semantic memory)

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

## Capability Delegation

`SubProvider` and `ModelOrchestrator` fully delegate capability queries to the underlying provider:

- `context_window()` — returns the actual context window size from the sub-provider. This is required for correct `auto_budget`, semantic recall sizing, and graph recall budget allocation when using the orchestrator.
- `supports_vision()` — returns `true` only when the active sub-provider supports image inputs.
- `supports_structured_output()` — returns the sub-provider's actual value.
- `last_usage()` and `last_cache_usage()` — delegate to the last-used provider. Token metrics are accurate even when the orchestrator routes across multiple providers within a session.

## Interactive Setup

Run `zeph init` and select **Multi-provider** as the LLM setup. The wizard prompts for:

1. **Primary provider** — select from Ollama, Claude, OpenAI, or Compatible. Provide the model name, base URL, and API key as needed.
2. **Fallback provider** — same selection. The fallback activates when the primary fails.
3. **Embedding model** — used for skill matching and semantic memory.

The wizard generates a complete `[[llm.providers]]` section with named entries and `embed`/`default` markers.

## Multi-Instance Example

Two Ollama servers on different ports — one for chat, one for embeddings:

```toml
[llm]

[[llm.providers]]
name = "ollama-chat"
type = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"
default = true

[[llm.providers]]
name = "ollama-embed"
type = "ollama"
base_url = "http://localhost:11435"       # second Ollama instance
embedding_model = "nomic-embed-text"      # dedicated embedding model
embed = true
```

## SLM Provider Recommendations

Each Zeph subsystem that calls an LLM exposes a `*_provider` config field. Matching the model size to task complexity reduces cost and latency without sacrificing quality. The table below lists the recommended model tier for each subsystem:

| Subsystem | Config field | Recommended tier | Rationale |
|-----------|-------------|-----------------|-----------|
| Skill matching | `[skills] match_provider` | Fast / SLM | Binary relevance signal; a 1.7B–8B model is sufficient |
| Tool-pair summarization | `[llm] summary_model` or `[llm.summary_provider]` | Fast / SLM | 1–2 sentence summaries; speed matters more than depth |
| Memory admission (A-MAC) | `[memory.admission] admission_provider` | Fast / SLM | Binary admit/reject decision; cheap models work well |
| MemScene consolidation | `[memory.tiers] scene_provider` | Fast / medium | Short scene summaries; medium model improves coherence |
| Compaction probe | `[memory.compression.probe] model` | Fast / medium | Question answering over a summary; Haiku-class is sufficient |
| Compress context (autonomous) | `[memory.compression] compress_provider` | Medium | Full compaction requires reasonable summarization quality |
| Complexity triage | `[llm.complexity_routing] triage_provider` | Fast / SLM | Single-word classification; any small model works |
| Graph entity extraction | `[memory.graph] extract_provider` | Fast / medium | NER + relation extraction; 8B models handle most cases |
| Session shutdown summary | `[memory] summary_provider` | Fast | Short session digest; latency is visible to the user |
| Orchestration planning | `[orchestration] planner_provider` | Quality / expert | Multi-step DAG planning requires high-capability models |
| MCP tool discovery (`Llm` strategy) | `[mcp.tool_discovery]` | Fast / medium | Relevance ranking from a short list |

A typical cost-optimized setup uses a local Ollama model (e.g., `qwen3:1.7b`) for all fast-tier subsystems and a cloud model (e.g., `claude-sonnet-4-6`) for quality-tier tasks:

```toml
[[llm.providers]]
name = "fast"
type = "ollama"
model = "qwen3:1.7b"
embed = true

[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
default = true

# Route cheap subsystems to the local model
[memory.admission]
admission_provider = "fast"

[memory.tiers]
scene_provider = "fast"

[memory.compression]
compress_provider = "fast"

[llm.complexity_routing]
triage_provider = "fast"

[orchestration]
planner_provider = "quality"
```

## Hybrid Setup Example

Embeddings via free local Ollama, chat via paid Claude API:

```toml
[llm]

[[llm.providers]]
name = "ollama"
type = "ollama"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"
embed = true

[[llm.providers]]
name = "claude"
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
default = true
```
