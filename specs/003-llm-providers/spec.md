---
aliases:
  - LLM Providers
  - Provider Trait
  - AnyProvider
tags:
  - sdd
  - spec
  - llm
  - providers
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec#3. LLM Provider Contract]]"
  - "[[022-config-simplification/spec]]"
  - "[[023-complexity-triage-routing/spec]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: LLM Providers

> [!info]
> LlmProvider trait, AnyProvider enum, prompt caching, debug request serialization,
> multi-provider pooling, chat vs chat_stream vs chat_with_tools codepaths.

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
    fn supports_tool_use(&self) -> bool;  // default: false
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
| Compatible | `compatible.rs` | OpenAI-compatible HTTP (LM Studio, vLLM, etc.); also used for GonkaGate Phase 1 |
| Candle | `candle.rs` | Local inference via HuggingFace candle (feature-gated) |
| Gemini | `gemini.rs` | Google Gemini API |
| Gonka (gateway) | `compatible.rs` | Phase 1: GonkaGate via `CompatibleProvider`; vault key `ZEPH_COMPATIBLE_GONKAGATE_API_KEY`; see [[051-gonka-gateway/spec]] |
| Gonka (native) | `gonka/provider.rs` | Phase 2: direct gonka network; ECDSA secp256k1 signing; `EndpointPool`; vault keys `ZEPH_GONKA_PRIVATE_KEY`, `ZEPH_GONKA_ADDRESS`; see [[052-gonka-native/spec]] |

## Prompt Caching (Claude only)

- System prompt split into blocks: `<!-- cache:stable -->`, `<!-- cache:tools -->`, `<!-- cache:volatile -->`
- Block 1 (stable): base identity + padding to ≥2048 tokens (Sonnet minimum)
- Block 2 (tools): serialized tool definitions, cached separately
- Block 3 (volatile): instruction files, skill context — changes every turn, not cached
- Max 4 `cache_control` breakpoints per request

### Configurable Prompt Cache TTL

`CacheTtl` controls the lifetime of Claude prompt-cache breakpoints. Configured per `[[llm.providers]]` entry:

```toml
[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
prompt_cache_ttl = "1h"   # "ephemeral" (default) or "1h"
```

| Value | Description | Beta header required |
|-------|-------------|---------------------|
| `"ephemeral"` | Standard Anthropic default cache TTL (~5 minutes). No beta header needed. | No |
| `"1h"` | Extended 1-hour TTL. Requires `extended-cache-ttl-2025-04-11` beta header. | Yes |

The `prompt_cache_ttl` field defaults to `None` (interpreted as `ephemeral`). Specifying `"1h"` adds the beta header automatically for the duration of that provider's requests.

`--migrate-config` preserves `prompt_cache_ttl = "1h"` and suppresses `ephemeral` (it is the default, so no migration write-back).

#### Key Invariants

- `CacheTtl::OneHour` requires the `extended-cache-ttl-2025-04-11` beta header — it MUST be added automatically whenever `prompt_cache_ttl = "1h"` is configured.
- `CacheTtl::Ephemeral` MUST NOT add the beta header — the header is only needed for non-default TTL.
- `prompt_cache_ttl` is a per-provider field. Different providers in `[[llm.providers]]` may have different TTLs.
- NEVER apply a `1h` TTL header to non-Claude providers — it is Claude API-specific.

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

## BetaHeaderRejected Retry (Claude)

When a Claude request fails with `LlmError::BetaHeaderRejected`, the provider automatically
retries the same request without the offending beta header. This retry covers all three call
paths: `chat_with_tools_stream`, `chat_typed`, and `chat_with_tools` (extended in commit #4591,
#4607). The retry is transparent to callers.

### Key Invariants

- Retry MUST cover ALL Claude call paths — single-path retry is incomplete
- Retry is Claude-specific — NEVER apply `BetaHeaderRejected` retry logic to other providers
- The retried request uses identical parameters except the beta header is omitted
- Retry happens at most once per request — no retry loop

## o-Series Models (OpenAI)

OpenAI o-series models (`o1`, `o2-*`, `o3`, `o4-mini`, `o4`, and any model matching the
`o` + digit prefix pattern) require `max_completion_tokens` in the API request body instead
of `max_tokens`. Zeph detects o-series models at request time via prefix match and uses the
correct field name automatically (commit #4591). This applies to:

- `chat` / `chat_stream` requests (uses `max_completion_tokens` when model is o-series)
- `chat_with_tools` requests

The o-series path also uses a separate `context_window` branch for context-length calculation
(#4811): when the model is o-series, context window is looked up from an internal table keyed
by model prefix rather than from the default `max_tokens` field.

### Key Invariants

- o-series detection uses model name prefix match — NEVER hardcode specific model names
- `max_tokens` must NOT be sent for o-series models — use `max_completion_tokens` only
- Non-o-series models continue to use `max_tokens`
- o-series `context_window` lookup uses the prefix table — NEVER fall back to the generic default

## Configurable SSE Buffer Caps (`StreamLimits`) (#4750, #4808, #4790)

`StreamLimits` in `LlmConfig` caps the size of SSE streaming buffers to prevent runaway
memory growth from malformed or oversized provider responses:

```toml
[llm.stream_limits]
max_tool_json_bytes = 4194304    # 4 MiB — tool call JSON accumulator
max_thinking_bytes = 1048576     # 1 MiB — thinking/reasoning block accumulator
max_compaction_bytes = 32768     # 32 KiB — compaction response accumulator
```

`ClaudeProvider` accepts `StreamLimits` via `.with_stream_limits()` builder. The fields are
wired through `LlmConfig`, `provider_factory`, and `src/init`. Migration step 56 injects a
commented `[llm.stream_limits]` hint into existing configs.

### Key Invariants

- `max_tool_json_bytes` default is 4 MiB — NEVER lower the default below 512 KiB (real tool schemas can be large)
- When a streaming buffer exceeds its cap, the stream is terminated with an error — NEVER silently truncate
- `StreamLimits` applies only to Claude streaming paths — other providers use equivalent internal caps

## Embedding Helpers in `LlmConfig` (#4850, #4840)

`effective_embedding_model` and `stable_skill_embedding_model` are moved from
`zeph_core::provider_factory` to `LlmConfig` in `zeph_config`, consistent with the
`stt_provider_entry` precedent. `impl Default for LlmConfig` is derived via serde
deserialization of an empty TOML string, so all `#[serde(default)]` fields produce a
well-typed default. All call sites in `src/bootstrap/` and `src/runner.rs` are updated
accordingly.

### Key Invariants

- `effective_embedding_model` and `stable_skill_embedding_model` live in `LlmConfig` — NEVER re-add them to `provider_factory`
- `LlmConfig::default()` must round-trip through empty TOML — NEVER implement `Default` by hand

## Key Invariants

- Provider methods are always `&self` — immutable, concurrent-safe. This scopes to **request/inference** methods (`chat`, `chat_stream`, `chat_with_tools`, …). In-place `&mut self` config setters applied only at turn boundaries between requests (e.g. `set_reasoning_effort`, and the runtime `set_thinking`/`set_thinking_budget`/`set_thinking_level`/`apply_reasoning_effort` setters from `[[specs/070-runtime-thinking-controls/spec]]`) are exempt — request methods remain `&self`.
- `debug_request_json()` must return exactly the JSON that would be sent to the API — used for debugging and testing
- `last_usage()` is updated after every call — must be accurate for cost tracking
- `chat`, `chat_stream`, `chat_with_tools` are independent codepaths — do not delegate one to another
- Candle and metal/cuda features are mutually exclusive in the build
- Provider identity is the `name` field from `[[llm.providers]]` — never resolved by type string
