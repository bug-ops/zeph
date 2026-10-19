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
| `crates/zeph-llm/src/claude/mod.rs` | Claude impl, `split_system_into_blocks`, prompt caching |
| `crates/zeph-llm/src/openai/mod.rs` | OpenAI impl |
| `crates/zeph-llm/src/ollama.rs` | Ollama impl |
| `crates/zeph-llm/src/compatible.rs` | OpenAI-compatible HTTP impl |
| `crates/zeph-llm/src/gemini/mod.rs` | Gemini impl |
| `crates/zeph-llm/src/router/` (`thompson.rs`, `reputation.rs`, `builder.rs`, `config.rs`) | Multi-provider routing, Thompson Sampling, EMA reputation |

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
AnyProvider { Ollama, Claude, OpenAi, Gemini, Candle (feature "candle"),
              Compatible, Router(Box<RouterProvider>), Triage(Box<TriageRouter>),
              Gonka (feature "gonka"), Cocoon (feature "cocoon") }
```

## Implementations

| Provider | File | Notes |
|---|---|---|
| Claude | `claude.rs` | Anthropic API, prompt caching (4 breakpoints), thinking blocks |
| OpenAI | `openai.rs` | OpenAI API + compatible endpoints |
| Ollama | `ollama.rs` | Local via `ollama-rs`, streaming; HTTP-status + transport-level retry (#6531) |
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
- The message-level breakpoint (`cache_user_messages`) sits on the last cache-capable block (`text` or `tool_result`) of the conversation tail, falling back to earlier blocks and messages when the tail ends in a thinking or `tool_use` block. A growing tool loop therefore reads the previous request's prefix from cache. The lookback window is 20 blocks, so an iteration that adds many blocks (about nine or more parallel tool calls) can miss the previous write; this costs cache hits, not correctness
- Within one tool loop the `tools` array, system blocks and every message before the tail are byte-identical from one request to the next, except across a deliberate history rewrite: pressure-driven edits (see `[[002-agent-loop/spec]]`), `complete_focus` / `compress_context` (the Knowledge block is a `Role::System` message hoisted into `system`), and a growing secret-masking registry that rewrites earlier text

### Configurable Prompt Cache TTL

`CacheTtl` controls the lifetime of Claude prompt-cache breakpoints. Configured per `[[llm.providers]]` entry:

```toml
[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-5"
prompt_cache_ttl = "1h"   # "ephemeral" (default) or "1h"
```

| Value | Description | Beta header required |
|-------|-------------|---------------------|
| `"ephemeral"` | Standard Anthropic default cache TTL (~5 minutes). No beta header needed. | No |
| `"1h"` | Extended 1-hour TTL. The `extended-cache-ttl-2025-04-25` beta header is sent only for models with the `extended_ttl_beta` capability (Legacy, Haiku45). | Legacy and Haiku45 only |

The `prompt_cache_ttl` field defaults to `None` (interpreted as `ephemeral`). Specifying `"1h"` adds the beta header automatically for models that still need it; newer and unknown models accept the TTL without it.

`--migrate-config` preserves `prompt_cache_ttl = "1h"` and suppresses `ephemeral` (it is the default, so no migration write-back).

#### Key Invariants

- The `extended-cache-ttl-2025-04-25` beta header MUST be added automatically when `prompt_cache_ttl = "1h"` is configured on a model whose `extended_ttl_beta` capability is set, and MUST NOT be sent to any other model (including `Unknown`).
- `CacheTtl::Ephemeral` MUST NOT add the beta header — the header is only needed for non-default TTL.
- `prompt_cache_ttl` is a per-provider field. Different providers in `[[llm.providers]]` may have different TTLs.
- NEVER apply a `1h` TTL header to non-Claude providers — it is Claude API-specific.

## Orchestrator

`crates/zeph-llm/src/router/` (`RouterProvider`, `thompson.rs`, `reputation.rs`) — multi-provider routing:

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
model = "claude-sonnet-5"
max_tokens = 4096
default = true
```

Subsystems reference a provider by name via a `*_provider` field. When the field is absent, the subsystem falls back to the default provider. See `.local/specs/024-multi-model-design/spec.md` for the full per-subsystem mapping.

## Ollama Retry: HTTP-Status and Transport-Level (#6531, #6491)

Unlike Claude/OpenAI/Gemini, `OllamaProvider::chat`/`chat_stream`/`chat_with_tools` originally
sent every request through `ollama-rs` with no retry — `ollama-rs` discards the HTTP status
code on non-2xx responses, so a single 429/503 or transient connection failure failed the whole
turn immediately. `ollama.rs` now posts chat requests directly via a dedicated `reqwest` client
(`ollama-rs`'s internal client is a different major `reqwest` version and not interchangeable
with `retry::send_with_retry`), reusing `ChatMessageRequest`/`ChatMessageResponse` only for
(de)serialization shape.

Two independent retry layers:

- **HTTP-status retry**: `retry::send_with_retry` (the same helper Claude/OpenAI/Gemini use)
  retries on real HTTP 429/503, honoring `Retry-After`. Max 3 attempts, matching the other
  backends' `MAX_RETRIES`.
- **Transport-level retry**: `send_with_transport_retry`, layered underneath, retries transient
  transport failures — request timeout, or a connection reset mid-request (`is_request()`
  without `is_connect()`) — that fail before a `reqwest::Response` exists to inspect, so they
  never reach `send_with_retry`'s status check. Same exponential backoff schedule as
  `send_with_retry`.

Connect-phase failures (`is_connect()`, e.g. no Ollama server running) are deliberately NOT
retried — they are far more likely to be permanent misconfiguration than a transient blip, and
`RouterProvider`'s fallback-exhaustion diagnostics depend on this failing fast.

The streaming NDJSON parser introduced alongside this change buffers raw bytes across chunk
boundaries and decodes only once a complete line is assembled, rather than decoding each chunk
independently — decoding per-chunk can silently drop a chunk that splits a multi-byte UTF-8
character.

### Key Invariants

- Ollama chat/streaming requests MUST retry on real HTTP 429/503 via `send_with_retry` — same
  retry semantics as Claude/OpenAI/Gemini
- Transport-level failures (timeout, mid-request connection reset) MUST be retried by
  `send_with_transport_retry` before status-level retry ever sees a response
- Connect-phase failures (e.g. server unreachable) MUST NOT be retried — fail fast to preserve
  `RouterProvider` fallback diagnostics
- The NDJSON stream parser MUST buffer across chunk boundaries and decode complete lines only —
  NEVER decode a chunk in isolation (risks silently dropping a split multi-byte character)

## Claude Model Capabilities, Window Ceiling and Server Compaction (#6822, #6825)

`ClaudeModel` (`claude/model.rs`) is a closed enum parsed once from the model id by `from_model_id`: provider prefixes (`anthropic.`, `us.anthropic.`), `@date` / `-v1:0` suffixes and `-YYYYMMDD` stamps are ignored, and the longest numeric version wins (`sonnet-5-5` is Sonnet 5.5, not Sonnet 5). `capabilities()` is one exhaustive match, so a new variant forces every capability to be decided. Variants: Haiku45, Haiku55, Sonnet46, Sonnet5, Sonnet55, Opus46, Opus47, Opus48, Opus5, Opus55, Fable5, Fable51, Mythos5, Mythos51, Legacy (anything older than 4.6 except Haiku 4.5), Unknown.

| Capability | Models |
|---|---|
| `prefers_effort` (`Extended` is converted to adaptive + `effort`, no `budget_tokens`) | Opus46-48, Opus5, Opus55, Sonnet5, Sonnet55, Haiku55, Fable, Mythos, Unknown |
| `rejects_prefill` | Sonnet46, Opus47+, all 5.x, Fable, Mythos, Haiku55, Unknown (Opus46 only while thinking is on) |
| `rejects_sampling` (`temperature` is never sent) | Opus47+, all 5.x, Fable, Mythos, Haiku55, Unknown |
| `thinking_always_on` (`max_tokens` is floored at 16 000 even without a thinking config) | Opus5, Opus55, Sonnet5, Sonnet55, Haiku55, Fable, Mythos |
| `forbids_forced_tool_choice` | Opus55, Sonnet55, Fable51, Mythos51, Unknown |
| `preserved_thinking` (`block_binding: drop_block`) | Fable51, Opus55, Sonnet55, Haiku55 |
| `server_compaction` | every 4.6+ model, Fable, Mythos, Haiku55; not Haiku45, Legacy or Unknown |
| `needs_interleaved_beta` (with `Extended` thinking and tools) | Sonnet46 |
| `extended_ttl_beta` (`extended-cache-ttl-2025-04-25` with `prompt_cache_ttl = "1h"`) | Legacy, Haiku45 |
| `structured_outputs` (`chat_typed` uses `output_config.format`) | Opus55, Sonnet55, Fable51, Mythos51 |
| `needs_1m_beta` (`context-1m-2025-08-07`) | Legacy only |
| `native_window` | 1M for 4.6+, 5.x, Fable, Mythos, Haiku55; 200k for Haiku45 and Legacy (1M with `needs_1m_beta`); none for Unknown |

`Unknown` is conservative: no `budget_tokens`, prefill or sampling parameters, `tool_choice: auto`, no implicit thinking parameter, no beta header, no server compaction, no context window.

### Context window ceiling

`ClaudeProvider::context_window()` returns the native window capped at `CLAUDE_DEFAULT_WINDOW_CEILING` (200 000, a `NonZeroU32`), so budgets stay at the long-standing 200k. `enable_extended_context = true` lifts the cap to the native window (1M on current models; Legacy gains it through the 1M beta, which is sent only when the flag is set and the model needs it; Haiku45 stays at 200k and warns). The ceiling is Claude-only: Gemini and OpenAI auto budgets are unchanged. A larger budget is also reachable via `memory.context_budget_tokens`.

### Server compaction wire (`compact-2026-01-12`)

- Request: `context_management: {"edits": [{"type": "compact_20260112", "trigger": {"type": "input_tokens", "value": N}, "pause_after_compaction": false}]}`. `N` is a `CompactionTrigger` (`zeph-config`, private field, `serde(try_from = "u32")`, minimum 50 000), set per provider with `server_compaction_trigger_tokens`. Unset means `max(50 000, 70% of context_window())` (140 000 at 200k), which stays below the tool-loop stop line (80% of the budget); a startup warning fires when an explicit trigger is at or above that line.
- Response block: `{"type": "compaction", "content": "..."}`. Streaming delivers one `content_block_delta` with `delta.type = "compaction_delta"` and the whole summary in `delta.content`, bounded by `max_compaction_bytes` and cut on a char boundary. A missing, null or empty summary yields no `Compaction` part and no `StreamChunk::Compaction`.
- `with_server_compaction(true)` is ignored with a warning on models without the `server_compaction` capability.
- `context_management` is sent only on `chat_with_tools` and the tools stream; plain `chat`, `chat_stream`, typed and vision requests never carry it, because they do not carry the full conversation history.
- `ChatResponse::Text { text, compaction }` carries the summary of a text-only reply exactly as `ToolUse` does; it is never dropped.
- `usage.iterations[]` entries of kind `compaction` are billed in addition to the top-level counters and exposed through `LlmProvider::last_auxiliary_usage()` (`AuxiliaryUsage`); the top-level counters stay the context-size value. Unknown iteration kinds are not billed (warning). The tracker resets the value on every recorded response.

### Beta latches

`BetaFeature::{Compact, ThinkingBinding}` each own an `AtomicBool` latch shared by provider clones. A 400 whose body names an active beta's header latches that beta off for the session (warn once) and the request is retried without it. The latch is the one-shot guard: a latched beta is no longer sent, so a repeat cannot name it. Any other 400 (including `context_management` validation errors) surfaces as an error. A 400 reporting a thinking block "bound to a different conversation" is classified first, as `LlmError::ThinkingBindingMismatch`, because that message also names `thinking-binding-controls-2026-08-01` when the header was absent and must not be read as a header rejection.

### Thinking binding wire

On `preserved_thinking` models (while not latched) the request carries `thinking: {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}}` (an explicit adaptive type when thinking is not configured, equal to the server default) and the `thinking-binding-controls-2026-08-01` beta. `block_binding` is only valid beside adaptive thinking. The response's `input_transformations` (top level, or `message_start.message` when streaming) is folded into `ThinkingReplay::{Intact, Dropped { first_message_index }}`; the index is in wire `messages[]` space, never greater than the history index, so stripping from it covers at least the dropped blocks. Recovery when thinking and history disagree: `ThinkingReplay::Dropped { first_message_index }` strips thinking from that index of the agent's history once (only for `prefix_binding_mismatch` and unknown reasons; `model_binding_mismatch` and `organization_binding_mismatch` drops leave history alone so a Router/Triage model switch does not wipe reasoning), before the response's own assistant message is pushed; `LlmError::ThinkingBindingMismatch` (beta latched off, unknown model, or a rewrite that escaped the seam) strips all thinking from history and retries the call once. `chat_typed` on `structured_outputs` models sends `output_config.format = {type: json_schema, schema}` (merged into the existing `output_config`, no tools, no `tool_choice`) and parses the joined text; the schema is normalized for the supported dialect (`additionalProperties: false` on every object, bound keywords and unsupported `format`s stripped), and a recursive `$ref` graph falls back to the tool path. On other `forbids_forced_tool_choice` models it sends `tool_choice: auto` plus a must-call system instruction and retries once with a user nudge; a second miss returns `StructuredParse`. `chat_typed` on the Router is the trait default (the router has no typed entry point), so a typed call through a router uses the system-prompt schema path.

### Key Invariants

- Capability decisions come only from `ClaudeModel::capabilities()` — NEVER from substring checks on the model id elsewhere
- NEVER send `budget_tokens`, a prefill turn or `temperature` to a model whose capability forbids it, and NEVER send `block_binding` without adaptive thinking
- NEVER read a thinking-binding mismatch as a beta-header rejection
- A `stop_reason: "refusal"` (non-stream, plain chat, typed, text SSE and tool SSE via `message_delta`) MUST surface as `LlmError::Refused { provider, category, explanation }`, never as an empty reply; partial output is discarded.
- `LlmError::forbids_provider_fallback()` (`InvalidInput` or `Refused`) MUST stop every router fallback loop (default strategies, cascade chat and stream, tools path); a refusal records no availability, reputation or Thompson outcome. Embedding paths keep `is_invalid_input()` because embeddings never refuse.
- Streamed thinking is bounded by `max_thinking_bytes` counting thinking text, `signature_delta` bytes and `redacted_thinking.data`; an oversized block is dropped whole together with every later thinking block of the response.
- Adjacent text blocks stay separate blocks on the stream (`ToolSseEvent::TextBlockStart` + `AssistantContentBuilder::begin_text_block`) and non-stream paths, so both produce identical `AssistantContent`.
- A beta latch flips at most once; the retry it triggers cannot loop
- NEVER store prefix-guard state in a shared instance atomic: the policy travels per request in `messages[0].metadata.request_policy` and fingerprints are keyed by `ConversationKey`

## Tool-Call ID Uniqueness (OpenAI/Compatible) (#6506, #6501)

`convert_messages_structured` (OpenAI provider; also used by `CompatibleProvider`, which
delegates message conversion to it) runs a `dedupe_tool_call_ids` pass at the end of
conversion. Some OpenAI-compatible backends derive their `tool_call.id` counter relative to
their own perceived context window; after Focus-based context compaction shrinks what Zeph
sends, the backend's counter can reset and re-mint an id that collides with one still present
earlier in the still-serialized history. A colliding `tool_calls[].id` is rejected outright by
the backend (`tool_calls[].id must be unique within the request`), stalling the turn.

`dedupe_tool_call_ids` walks the outgoing request in order, rewrites any repeated
`tool_calls[].id` to a freshly-minted unique id, and keeps the paired `tool`-role message's
`tool_call_id` in sync (FIFO pairing per original id, so multiple collisions on the same
original id resolve to the correct occurrence).

### Key Invariants

- Every `tool_calls[].id` in a single outgoing request MUST be unique before serialization —
  this is a hard wire-format requirement of the OpenAI-compatible API, not an optional
  cleanliness pass
- Ids that never collide MUST be left untouched — only colliding ids are rewritten
- Rewriting a `tool_calls[].id` MUST update the paired `tool`-role message's `tool_call_id` to
  match — an unpaired rewrite corrupts call/response matching
- Dedup applies to both `openai` and `compatible` provider types, since `CompatibleProvider`
  delegates message conversion to `OpenAiProvider`
- Zeph does not mint `tool_call.id` values itself — it replays whatever the upstream model
  returned as history; the fix is defensive rewriting on the outbound path, not a change to id
  generation

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

## Tool-Pair Repair (`zeph_llm::tool_pairing`) (#6771, #6770, #6769, #6762, #6781, #6783)

`crates/zeph-llm/src/tool_pairing.rs` is the single, shared definition of "orphaned"
`ToolUse`/`ToolResult` pairing and repair, used by **both** the Claude request builder's
per-request hot path (`claude/request.rs`) and the OpenAI/OpenAI-compatible request builder's
per-request hot path (`openai/mod.rs`'s `convert_assistant_tool_message`/
`push_user_tool_messages`, issue #6781) — previously only Claude had a request-build-time
backstop, so an orphan surviving upstream trim/sanitize passes could reach OpenAI or an
OpenAI-compatible/Ollama endpoint unrepaired and produce a 400/422. Also used by every
history-mutating call site that previously implemented its own copy:
`zeph-core::agent::subagent_commands::trim_parent_messages`,
`zeph-core::agent::shutdown::flush_orphaned_tool_use_on_shutdown`,
`zeph-subagent::agent_loop::trim_message_history`, and
`zeph-agent-persistence::sanitize::sanitize_tool_pairs`. Prior to this consolidation each site
independently reimplemented pairing, and one (`trim_parent_messages`, issue #6770) matched
against a **global** id set rather than adjacency, cross-pairing an orphan against an unrelated
call sharing its id under Ollama-style `format!("call_{i}")` id reuse.

Request-time repair coverage per provider: Claude, OpenAI family, and Gemini = Layer A over the
`AgentVisible` projection (#6814); native Ollama = N/A (no tool-call ids on the wire).

### Agent visibility (`zeph_llm::provider::AgentVisible`) (#6814)

Messages with `MessageVisibility::UserOnly` (originals replaced by a deferred tool-pair summary)
and `MessageVisibility::Marker` (bookkeeping such as the focus checkpoint) must never reach a
provider. `AgentVisible<'a>` is the borrowed, filtered projection; its only
constructors filter (`AgentVisible::new`, `From<&[Message]>`), and exposes `iter`/`len`/`get`/
`next_non_system`/`prev_non_system`. Every request builder converts through it: OpenAI
`convert_messages`/`_vision`/`_structured`/image detection/`debug_request_json`, Gemini
`convert_messages` + tool-name lookup, native Ollama `chat`/`chat_stream`/`chat_with_tools`/
`debug_request_json`, Candle `ChatTemplate::format`, and Claude `split_messages*`. Emission and
Layer A classification share the same view, so a pair straddling a hidden message is classified
as it is emitted. `tool_pairing::{next,prev}_non_system` are generic over `M: Borrow<Message>`.
Downgrade wording is shared: `tool_pairing::{downgraded_tool_use_text, downgraded_tool_result_text}`
(Claude, OpenAI, Gemini). Gemini renders an unmatched `ToolUse`/orphaned `ToolResult` as a text part.

Three layers, plus one prevention primitive:

- **`pair_safe_cut(messages, cut, floor)`** — cut *prevention* (#6782): places history-removal
  cuts (compaction drain, budget trim, memory-first drain) so they never separate a `ToolUse`
  from its `ToolResult`. Presence-based by design (an in-flight `ToolUse` counts as paired); it
  prevents orphans rather than classifying them, so it is the one sanctioned exception to the
  "no second pairing predicate" invariant below.

- **Layer A** — `unmatched_tool_use_ids(msg, next)` / `unmatched_tool_result_ids(msg, prev)` /
  `resolved_tool_result_ids(next)`: pure classification, adjacency-scoped against an
  already-resolved neighbour message (works for both `Vec<Message>` and `Vec<&Message>` callers).
  `resolved_tool_result_ids` is the "matched" half of `unmatched_tool_use_ids` factored out so a
  caller testing arbitrary candidate ids (not necessarily drawn from a `Message`'s own `parts`)
  against the same adjacency-scoped resolution set does not reimplement the predicate — used by
  `zeph-core::agent::tool_execution::focus::persist_cancelled_tool_results`'s idempotency guard
  (issue #6783), which checks a caller-supplied `&[ToolUseRequest]` batch rather than an assistant
  message's own parts.
- **`next_non_system(messages, at)` / `prev_non_system(messages, at)`** — the single shared
  definition of "next/previous adjacent message", skipping any `Role::System` messages in
  between; used by every call site that classifies orphans against `&[Message]` directly rather
  than a pre-filtered `visible` slice (contrast `claude::request::split_messages_structured`,
  which pre-filters agent-visible non-system messages and indexes that slice instead).
- **Layer B** — `repair_tool_pairs` / `repair_window`: mutating repair built strictly on top of
  Layer A, parameterized by `OrphanAction` (`Delete` or `DowngradeToText`) for orphaned
  `ToolResult` handling; orphaned `ToolUse` is always deleted regardless of `action`.
  `repair_window` additionally enforces that the first non-system message has role `user`
  (required by the Anthropic API), looping the pairing repair and the leading-role drop to a
  joint fixed point since dropping a leading non-user message can newly orphan a pair that was
  otherwise well-formed.

### Key Invariants

- Pairing is always adjacency-scoped (immediately preceding/following non-system message via
  `next_non_system`/`prev_non_system`) — a global id set across the whole history is NEVER an
  acceptable match, since it produces the #6770 cross-pair defect under any provider that reuses
  `tool_use_id`s across turns
- Layer B MUST call Layer A for every orphan decision — no second, independent pairing predicate
  may exist anywhere in the module, other than `pair_safe_cut`'s presence-based cut placement
  (this is a reviewer-verified property of the source, not a grep-checkable pattern)
- The Claude `GatedStructuredHistory`/`GatedPlainHistory` are empty or lead with a `user`
  message by construction: a non-`user` leading turn gets a `LEADING_USER_ANCHOR` user message
  prepended in `new` (after the cache-breakpoint cap and the no-prefill strip) — messages are
  never dropped to satisfy this, so no tool pair or compaction block is severed. An all-assistant
  history under no-prefill stays empty (a loud API 400, not a fabricated request)
- An orphaned `ToolUse` part is always deleted, never downgraded — it has no safe text form
  worth preserving as conversation content
- `Role::System` messages are never removed by repair, regardless of content or parts state
- The Claude request builder's `StructuredApiMessage` list must never contain an entry with an
  empty block array — Anthropic rejects an empty content array. This applies to every producer of
  an empty block list (an orphaned empty-content `ToolResult`, or a message whose only parts are
  `ThinkingBlock`/`RedactedThinkingBlock`/`Compaction`/whitespace-only `Text` on the user role),
  not only the tool-pairing case
- The `[tool result: {id}] {content}` downgrade marker (space, no underscore) MUST stay
  distinguishable from `has_meaningful_content`'s recognized `[tool_result: ` marker (underscore)
  — a downgraded orphan must always read as "meaningful" content so it is never later mistaken
  for empty, marker-only text and dropped as the window's leading anchor
- An idempotency guard that checks "is this tool call already resolved" (e.g.
  `persist_cancelled_tool_results`) MUST be adjacency-scoped via `resolved_tool_result_ids`
  against the message adjacent to the current turn boundary — NEVER a scan of the whole turn
  tail, which a later, unrelated `ToolResult` reusing an orphaned call's id (Ollama-style
  batch-index id reuse) can fool into wrongly treating the orphan as already resolved and
  silently skipping its shutdown tombstone (#6783). When no turn-boundary `Role::Assistant`
  message exists in history at all, treat every id as unresolved rather than guessing a
  boundary — the safe failure direction is a spurious duplicate tombstone, never a silently
  dropped one.

## Claude Message Coalescing and Compaction Rendering (#6815)

- `split_messages_structured` coalesces consecutive same-role `StructuredApiMessage`s into one multi-block message, deterministically, after Layer A downgrade and before the cache breakpoint/anchor. Dropped/hidden messages and a compaction block followed by assistant output otherwise leave adjacent same-role entries
- Coalescing is order-preserving: blocks of merged messages are concatenated in message order, so thinking stays ahead of the text and `tool_use` blocks that followed it
- Request-side thinking scope: `messages[0].metadata.request_policy` (`RequestPolicy { thinking: ThinkingScope, conversation: ConversationKey }`, stamped by the agent before every send, never persisted) wins. `None` keeps the model-based scope: on `preserved_thinking` models only assistant messages after the last `MessageOrigin::TurnInput` carry thinking blocks (`ThinkingScope::CurrentTurn`), other models forward thinking from every turn, and without a turn-input marker nothing is dropped. `Some(Verbatim)` (pinned `append` conversation, `[[088-prompt-cache-prefix-stability/spec]]`) forwards thinking from every turn and enables the prefix guard
- Tool responses carry `ChatResponse::ToolUse { content: AssistantContent, replay: ThinkingReplay }`: the thinking, text and `tool_use` blocks in the order the model produced them (streaming: arrival order), plus the provider's `input_transformations` verdict. The agent stores the blocks verbatim in one assistant message and never persists thinking
- After a server-compaction mirror the request carries `system` (the System messages), an `assistant` turn starting with the `compaction` block, then the turn input; recall injected later lands after the compaction block (`context_floor`)
- Gemini renders a `MessagePart::Compaction` part as a plain text part (previously dropped, leaving an empty turn)

## Prefix Guard, Mid-Conversation System Directives and Typed Path (#6846, #6848)

- `MessagePart::TurnContext` is rendered by every content branch through `Message::to_request_text`. Claude: `Retrieved` is a user text block before the user text; `Directives` is a wire `role:"system"` message only when the turn input is the last visible message or is directly followed by an assistant message, otherwise a user text block after the user text (an abandoned turn input coalesces with the next turn as plain text, so no thinking is at risk). Other providers render both blocks as user text
- `ClaudeModel.mid_conversation_system` (Opus 5, Opus 5.5, Opus 4.8, Fable 5, Fable 5.1, Mythos 5/5.1, Sonnet 5.5) gates the system rendering; a 400 "not supported" latches the user-text fallback once, like the beta latches. `apply_cache_breakpoint` targets the last wire message that is not a mid-conversation system message
- Prefix guard (only when `request_policy` is `Some`): after building a `chat_with_tools` or tool-stream request the provider hashes the wire projection (system blocks, tools, each wire message, `cache_control` removed) plus the wire-to-history index map and compares it with the fingerprint stored under the same `ConversationKey` (`prefix_fingerprints`: bounded `VecDeque`, capacity 16, oldest evicted, clones start empty). Divergence at wire index `k` (system or tools change means `k = 0`) drops thinking from `k` in the payload as a trailing run and reports `ThinkingReplay::Dropped { first_message_index: history_idx(k) }`, merged by minimum index with the server verdict (tool stream: a synthetic first `ToolSseEvent::ThinkingReplay`). The fingerprint is stored after the thinking drop and committed once the provider accepts the response (HTTP 200 for streams, before the stream is consumed); each internal retry recomputes it from the body it sends. One `llm.claude.prefix_guard{reason, wire_index, history_index}` span per divergence, none in steady state. One-shot `chat` calls do not touch the fingerprints
- `LlmProvider::prefix_cache() -> PrefixCache { None, Prefix }`: Claude is `Prefix` when `cache_user_messages`; Router and Triage are `Prefix` if any sub-provider is (same aggregation as `thinking_binding`); Masked and Any delegate
- Streamed usage: `StreamUsage` merges `message_start` and `message_delta` into one `ApiUsage`, recorded through the shared `record_api_usage` after each (also calls `log_cache_usage`); compaction iterations count as auxiliary usage. `clear_response()` clears usage, cache, auxiliary and reasoning state but not `last_ttft`, so a stream failing before `message_start` leaves no stale values
- Typed path: `LlmProvider::chat_typed` returns `impl Future<Output = Result<T, LlmError>> + Send` (`T: DeserializeOwned + JsonSchema + Send + 'static`, `Self: Sized`); `LlmProviderDyn::chat_typed_boxed` and the `AnyProvider`/`MaskedProvider` overrides reach native overrides (`output_config.format` through Masked, with masked content). `chat_typed_erased` is removed

## Runtime Note Rendering (#6786, #6817)

`MessagePart::RuntimeNote` (spec 001 section 4) is rendered per builder: Claude emits a `text` block after the `tool_result` blocks of the tail `user` message; OpenAI-family emits a `user` message directly after the `tool` messages (`push_user_tool_messages`); Gemini adds a text part after the `functionResponse` parts of the same merged `user` content (live-verify acceptance); Ollama `convert_message_structured` returns `Vec<ChatMessage>` (tool message, then `user` note message); Candle and the plain (non-tool) paths use `Message::to_request_text`. Plain-path `ApiMessage.content` is `Cow<str>` for that reason. Caching: notes are appended to the last message only, so the cached prefix (tools, system, earlier messages) is unchanged within a tool loop; the k to k+1 prefix is byte-identical only while tool_filter, pruning, deferred summaries and soft compaction are inactive. In the `replace` layout the per-turn system rebuild (skills bodies, volatile block) still busts the message cache once per turn; the `append` layout removes it (#6846, #6820).

## Key Invariants

- A request builder MUST go through `AgentVisible` — never iterate a raw `&[Message]` for emission or adjacency
- Provider methods are always `&self` — immutable, concurrent-safe. This scopes to **request/inference** methods (`chat`, `chat_stream`, `chat_with_tools`, …). In-place `&mut self` config setters applied only at turn boundaries between requests (e.g. `set_reasoning_effort`, and the runtime `set_thinking`/`set_thinking_budget`/`set_thinking_level`/`apply_reasoning_effort` setters from `[[specs/070-runtime-thinking-controls/spec]]`) are exempt — request methods remain `&self`.
- `debug_request_json()` must return exactly the JSON that would be sent to the API — used for debugging and testing
- `last_usage()` is updated after every call — must be accurate for cost tracking
- `chat`, `chat_stream`, `chat_with_tools` are independent codepaths — do not delegate one to another
- Candle and metal/cuda features are mutually exclusive in the build
- Provider identity is the `name` field from `[[llm.providers]]` — never resolved by type string
