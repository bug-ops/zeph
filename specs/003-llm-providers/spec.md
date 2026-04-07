# Spec: LLM Providers

## Sources

### External
- Claude prompt caching: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
- Claude API overview: https://platform.claude.com/docs/en/api/overview
- Claude Context Management & Compaction: https://platform.claude.com/docs/en/build-with-claude/context-management
- Gemini API (text generation): https://ai.google.dev/gemini-api/docs/text-generation
- Gemini API (embeddings): https://ai.google.dev/gemini-api/docs/embeddings
- Gemini API (function calling): https://ai.google.dev/gemini-api/docs/function-calling
- Gemini API (models): https://ai.google.dev/gemini-api/docs/models
- OpenAI API reference: https://platform.openai.com/docs/api-reference/chat
- OpenAI Structured Outputs: https://platform.openai.com/docs/guides/structured-outputs
- **RouteLLM** (ICML 2024) — Thompson Sampling model routing: https://arxiv.org/abs/2406.18665

### Internal
| File | Contents |
|---|---|
| `crates/zeph-llm/src/provider.rs` | `LlmProvider` trait, `Message`, `MessagePart`, `ChatResponse` |
| `crates/zeph-llm/src/any.rs` | `AnyProvider` enum dispatch |
| `crates/zeph-llm/src/claude.rs` | Claude impl, `split_system_into_blocks`, prompt caching |
| `crates/zeph-llm/src/openai.rs` | OpenAI impl |
| `crates/zeph-llm/src/ollama.rs` | Ollama impl |
| `crates/zeph-llm/src/compatible.rs` | OpenAI-compatible HTTP impl |
| `crates/zeph-llm/src/gemini.rs` | Gemini impl |
| `crates/zeph-llm/src/orchestrator.rs` | Multi-provider routing, Thompson Sampling, EMA |

---

`crates/zeph-llm/` — provider abstraction + concrete implementations.

## Provider Trait

```rust
trait LlmProvider: Send + Sync {
    fn chat(&self, messages: &[Message]) -> impl Future<Output = Result<String, LlmError>> + Send;
    fn chat_stream(&self, ...) -> impl Future<Output = Result<ChatStream, LlmError>> + Send;
    fn chat_with_tools(&self, messages, tools) -> impl Future<Output = Result<ChatResponse, LlmError>> + Send;
    fn embed(&self, text: &str) -> impl Future<Output = Result<Vec<f32>, LlmError>> + Send;
    fn supports_streaming(&self) -> bool;
    fn supports_embeddings(&self) -> bool;
    fn supports_tool_use(&self) -> bool;
    fn supports_vision(&self) -> bool;
    fn supports_structured_output(&self) -> bool;
    fn debug_request_json(&self, ...) -> serde_json::Value;
    fn name(&self) -> &str;
    fn last_usage(&self) -> Option<(u64, u64)>;  // (input_tokens, output_tokens)
}
```

## AnyProvider Enum

Runtime dispatch — no `Box<dyn LlmProvider>` in hot paths:

```
AnyProvider { Claude, OpenAI, Ollama, Compatible, Candle, Gemini }
```

## Implementations

| Provider | File | Notes |
|---|---|---|
| Claude | `claude.rs` | Anthropic API, prompt caching (4 breakpoints), thinking blocks |
| OpenAI | `openai.rs` | OpenAI API + compatible endpoints |
| Ollama | `ollama.rs` | Local via `ollama-rs`, streaming |
| Compatible | `compatible.rs` | OpenAI-compatible HTTP (LM Studio, vLLM, etc.) |
| Candle | `candle.rs` | Local inference via HuggingFace candle (feature-gated) |
| Gemini | `gemini.rs` | Google Gemini API |

## Prompt Caching (Claude only)

- System prompt split into blocks: `<!-- cache:stable -->`, `<!-- cache:tools -->`, `<!-- cache:volatile -->`
- Block 1 (stable): base identity + padding to ≥2048 tokens (Sonnet minimum)
- Block 2 (tools): serialized tool definitions, cached separately
- Block 3 (volatile): instruction files, skill context — changes every turn, not cached
- Max 4 `cache_control` breakpoints per request

## Orchestrator

`crates/zeph-llm/src/orchestrator.rs` — multi-provider routing:

- **Rule-based routing**: match provider by name pattern, task type, or cost threshold
- **Thompson Sampling router**: Beta-distribution exploration/exploitation for model selection
- **EMA latency routing**: exponential moving average latency to prefer fastest provider
- **Fallback chain**: if primary fails, try next in configured order

## Config Format

All providers are declared via `[[llm.providers]]` in the TOML config — one entry per provider, no duplication across sections. See `.local/specs/022-config-simplification/spec.md` for the full `ProviderEntry` schema and examples.

```toml
[llm]
routing = "cascade"   # none | ema | thompson | cascade | task

[[llm.providers]]
name = "fast"
type = "openai"
model = "gpt-4o-mini"
embedding_model = "text-embedding-3-small"
embed = true

[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
default = true
```

Subsystems reference a provider by name via a `*_provider` field. When the field is absent, the subsystem falls back to the default provider. See `.local/specs/024-multi-model-design/spec.md` for the full per-subsystem mapping.

## ASI: Agent Stability Index

`crates/zeph-llm/src/router/asi.rs` — per-provider coherence tracking. Implemented in v0.18.5.

### Overview
`AsiState` maintains a sliding window of response embeddings per provider. Coherence is computed as cosine similarity of the latest embedding vs. the window mean. Low coherence penalizes Thompson beta priors and EMA scores via `penalty_weight`. State is session-only (no persistence). The embed call is fire-and-forget via `tokio::spawn` — routing is never blocked by it.

### Config
```toml
[llm.routing.asi]
enabled = false
window_size = 5            # sliding window depth
coherence_threshold = 0.5  # warn when coherence drops below this
penalty_weight = 0.3       # how much low coherence reduces Thompson/EMA scores
```

### Key Invariants
- `coherence()` returns `1.0` until at least 2 embeddings are observed — no penalty during warm-up
- ASI does not block routing — embed call is fire-and-forget; lag of 1–2 responses is acceptable
- Session-only state — never persisted; ASI resets on every process restart
- NEVER use ASI state from a different provider's window — per-provider isolation is mandatory

---

## Quality Gate for Thompson/EMA Routing

Optional post-selection embedding similarity check. After a provider is selected and produces a response, `cosine_similarity(query_emb, response_emb)` is computed. If similarity is below the threshold, the next provider in the ordered list is tried. On full exhaustion, the best response seen is returned (no `NoProviders` error). Fail-open on embed errors.

Does not apply to Cascade or Bandit routing strategies.

### Config
```toml
[llm.routing]
quality_gate = 0.75   # omit or set to 0.0 to disable; values > 1.0 are silently ignored
```

### Key Invariants
- `quality_gate > 1.0` is silently ignored — not wired into the router
- On embed error during gate evaluation, the current response is accepted (fail-open)
- NEVER apply quality_gate to Cascade or Bandit strategies

---

## Per-Provider Cost Breakdown

`crates/zeph-core/src/cost.rs` — per-provider token and cost tracking. Implemented in v0.18.5.

### Overview
`CostTracker::record_usage` accepts `provider_name`, `cache_read_tokens`, and `cache_write_tokens` in addition to input/output tokens. Cache pricing is applied per-provider type:
- Claude: cache read = 10% of prompt price, cache write = 125% of prompt price
- OpenAI: cache read = 50% of prompt price
- Others: 0%

Per-provider totals (input, cache_read, cache_write, output tokens, cost, request count) are accumulated in `CostState::providers` and exposed via `CostTracker::provider_breakdown()`.

`MetricsSnapshot` gains `provider_cost_breakdown: Vec<(String, ProviderUsage)>`. The `/status` CLI command and TUI `/cost` view both render a per-provider table sorted by cost descending. Daily reset clears the breakdown alongside the spending total.

### Key Invariants
- `record_usage` must always pass `provider_name` — anonymous usage cannot be attributed
- Cache pricing constants are per-provider-type, not per-named-provider — mapping is by type string
- Daily reset clears per-provider breakdown atomically with the spending total
- NEVER attribute cache tokens to a different provider than the one that produced them

---

## `spawn_asi_update` Debounce

`RouterProvider` tracks a `turn_counter` (`Arc<AtomicU64>`) incremented once at the top of `chat()`. `spawn_asi_update` uses a second atomic (`asi_last_turn`) to gate on the current `turn_id` via `swap(AcqRel)` — concurrent sub-calls within the same turn (tool schema fetches, streaming sub-calls) are dropped. Exactly one embed call and one ASI window update fire per turn.

### Key Invariant
- NEVER fire `spawn_asi_update` more than once per logical agent turn — multiple concurrent `chat()` calls within a turn share the same `turn_id` gate

---

## Key Invariants

- Provider methods are always `&self` — immutable, concurrent-safe
- `debug_request_json()` must return exactly the JSON that would be sent to the API — used for debugging and testing
- `last_usage()` is updated after every call — must be accurate for cost tracking
- `chat`, `chat_stream`, `chat_with_tools` are independent codepaths — do not delegate one to another
- Candle and metal/cuda features are mutually exclusive in the build
- Provider identity is the `name` field from `[[llm.providers]]` — never resolved by type string
- Providers with `embed = true` are excluded from EMA/Thompson/Cascade/Bandit routing pool — embedding-only providers must not receive chat completion requests
