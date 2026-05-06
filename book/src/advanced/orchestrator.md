# Model Orchestrator

> **Tip:** For simple fallback chains with adaptive routing (Thompson Sampling or EMA), use `routing = "cascade"` or `routing = "thompson"` in `[llm]` instead. See [Adaptive Inference](adaptive-inference.md).

> **Note:** `routing = "task"` was removed as unimplemented in #3248. If your config uses it, `--migrate-config` will drop it with a warning and fall back to default single-provider routing.

Use a multi-provider setup to combine local and cloud models — for example, embeddings via Ollama and chat via Claude. Provider selection is controlled via `default = true` and `embed = true` markers.

## Configuration

```toml
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

## Orchestration-Tier Provider Routing

Sub-agent orchestration runs several internal LLM tasks that are distinct from user-facing reasoning:

- **Scheduling and aggregation** — combining multiple sub-agent outputs into a coherent result
- **Predicate evaluation** — deciding whether a task completed successfully (true/false classifiers)
- **Task verification** — double-checking a result before returning it to the user

These tasks can often be handled by smaller/faster models without impacting overall quality. The `orchestrator_provider` field routes all three through a single dedicated provider:

```toml
[[llm.providers]]
name = "fast"
type = "ollama"
model = "qwen3:1.7b"

[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
default = true

[orchestration]
orchestrator_provider = "fast"      # Use fast model for scheduling-tier LLM calls
planner_provider = "quality"         # Use quality model for planning (stays on quality provider)
```

The resolution order is:

- `LlmAggregator` (output synthesis) → `orchestrator_provider` → primary
- `PlanVerifier` (verification check) → `verify_provider` → `orchestrator_provider` → primary
- `PredicateEvaluator` (predicate logic) → `predicate_provider` → `orchestrator_provider` → primary

When `planner_provider` is explicitly set, it is NOT overridden by `orchestrator_provider`. Planning is a complex task and always uses the quality provider.

> [!WARNING]
> Routing `LlmAggregator` through a cheap/fast model may reduce final output quality because aggregation produces user-visible text. Test thoroughly with your workload before relying on this optimization in production.

## Admission Control and Concurrency Limits

To prevent provider overcommit when many sub-agents are running, set `max_concurrent` per provider. This limits the number of simultaneous in-flight orchestration calls to that provider:

```toml
[[llm.providers]]
name = "api"
type = "openai"
model = "gpt-4o"
max_concurrent = 10      # Allow up to 10 concurrent sub-agent API calls

[[llm.providers]]
name = "local"
type = "ollama"
model = "qwen3:8b"
max_concurrent = 4       # Ollama server has less capacity
```

The `AdmissionGate` enforces these limits at spawn time. When a provider reaches its limit, new tasks are deferred with exponential backoff until a previous task completes and frees a permit.

Currently the concurrency limit is enforced (tasks are delayed), but cost budgets are warn-only: when a task completes with token usage exceeding `[orchestration] default_task_budget_cents`, a warning is logged but the task is not rejected. Hard budget enforcement is deferred pending per-task `CostTracker` scoping.

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
