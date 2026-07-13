---
aliases:
  - Multimodal MCP Passthrough Spec
  - Spec 072
  - MCP Image Passthrough
tags:
  - sdd
  - spec
  - mcp
  - llm
  - security
  - tools
created: 2026-07-13
status: draft
related:
  - "[[001-system-invariants/spec]]"
  - "[[008-1-lifecycle]]"
  - "[[008-3-security]]"
  - "[[010-2-injection-defense]]"
  - "[[024-multi-model-design]]"
  - "[[040-content-sanitizer]]"
  - "[[069-threat-model/spec]]"
  - "[[068-session-persistence/spec]]"
  - "[[MOC-specs]]"
issues:
  - "#5366"
---

# Spec 072 — Multimodal MCP `ContentBlock` Passthrough to Vision-Capable Providers

> [!info]
> Lets a vision-capable LLM provider actually *see* an image an MCP tool returns, instead of
> the current text-only placeholder (`[image: mime, N bytes]`). Scope v1 to images only,
> opt-in per MCP server, default OFF. Resolves GitHub issue #5366.

## Sources

### External
- [Model Context Protocol specification — `ContentBlock`](https://modelcontextprotocol.io) — the union type MCP tool results already return (`Text`, `Image`, `Audio`, `Resource`, `ResourceLink`).
- [Anthropic Messages API — image content blocks](https://docs.claude.com/en/docs/build-with-claude/vision) — reference for how `MessagePart::Image` is expected to serialize.

### Internal

| File | Contents |
|---|---|
| `crates/zeph-mcp/src/content.rs` | `render_content_block`/`render_content_blocks` — text-only flattening of `rmcp::model::ContentBlock`, including the `Image` arm (`:58-60`) that currently discards the bytes |
| `crates/zeph-mcp/src/executor.rs` | `McpToolExecutor::execute_tool_call` (`:96-137`) — decode hook point; sets `ClaimSource::Mcp` |
| `crates/zeph-tools/src/executor.rs` | `ToolOutput` struct (`:267-`, 10 fields, no `Default`); `ToolCall` (`:49-`, already `#[derive(..., Default)]` with a `ToolName` field — proves `ToolName: Default`) |
| `crates/zeph-core/src/agent/tool_execution/mod.rs` | `ToolResultClassification` struct (`:88-98`) — the real per-tool-result carrier between classification and message-part construction |
| `crates/zeph-core/src/agent/tool_execution/tool_result.rs` | `classify_tool_result` (`:266-360`); `process_one_tool_result` (`:369-477`, pushes `MessagePart::ToolResult` at `:471-475`) — **the real production per-tool-result path** |
| `crates/zeph-core/src/agent/tool_execution/tier_loop.rs` | `process_tool_result_batch` (`:2478-2667`) — batch orchestrator calling `process_one_tool_result` in a loop, building `result_parts: Vec<MessagePart>`, then `Message::from_parts(Role::User, result_parts)` and `persist_message` |
| `crates/zeph-core/src/agent/mod.rs` | `build_user_message` (`:1732-1760`) — existing `supports_vision()` gate template for user-uploaded images |
| `crates/zeph-core/src/agent/message_queue.rs` | `detect_image_mime` (`:25-36`, unknown→`image/png` fallback, no magic-byte check); `MAX_IMAGE_BYTES = 20 MiB` (`:14`) |
| `crates/zeph-llm/src/provider.rs` | `MessagePart::Image(Box<ImageData>)` (`:307`); `ImageData { data: Vec<u8>, mime_type: String }` (`:348-352`, derives `Debug` over raw bytes); `LlmProvider::supports_vision()` (`:843`) |
| `crates/zeph-llm/src/claude/request.rs` | `:111-121` — `MessagePart::Image` already recognized as a "structured part" alongside `ToolResult` in the same user message; `:404`, `:962` — serialization |
| `crates/zeph-llm/src/router/triage.rs` | `supports_vision()` (`:641-643`) — `self.tier_providers.iter().any(...)`, the router aggregation gap |
| `crates/zeph-agent-persistence/src/embed.rs` | `serialize_parts_json` (`:122-137`) — unconditional `serde_json::to_string(parts)`, the SQLite persist surface |
| `crates/zeph-agent-persistence/src/hydrate.rs` | `:285,338,359,420` — reconstructs only `Text`/`ToolUse`/`ToolResult` parts; `Image` parts are never rehydrated (structural, not a control) |
| `crates/zeph-agent-persistence/src/session_sink.rs` | `SessionSink::record_message`/`record_user_message` — durable JSONL dual-write, invoked **before** `PersistenceService::persist_message` |
| `crates/zeph-core/src/agent/persistence/store.rs` | `Agent::persist_message` shim (`:24-`) — calls `sink.record_message` (`:57`) then `svc.persist_message` (`:89-`); the correct single strip point (see §4, C1) |
| `crates/zeph-config/src/channels.rs` | `McpTrustLevel` (`:21-`, `Trusted`/`Untrusted`/`Sandboxed`); `McpServerConfig` (`:1344-`); `McpConfig` (`:1209-`, global `[mcp]` section) |
| `src/init/mcp.rs` | `--init` wizard MCP server prompts |
| `crates/zeph-config/src/migrate/mod.rs` | `--migrate-config` step registry |

---

## 1. Overview

### Problem Statement

`McpToolExecutor::execute_tool_call` flattens every `rmcp::model::ContentBlock` returned by an
MCP tool — including `ContentBlock::Image` — into a text placeholder
(`render_content_block`, `content.rs:58-60`: `"[image: {mime}, {n} bytes]"`). The raw image
bytes are discarded. A vision-capable provider (Claude, OpenAI, Gemini, Ollama with a vision
model) therefore never sees an image an MCP tool actually returned — e.g. a screenshot tool, a
document-render tool, or a chart-generation tool — even though the provider is fully capable of
interpreting it. This was an explicitly deferred follow-up from the rmcp 2.0 migration.

### Goal

An MCP server, once explicitly opted in by the operator, can return an image in a tool result
and have that image attached as a native `MessagePart::Image` sibling part in the same turn,
visible to a vision-capable provider — with the same untrusted-content threat-model rigor
already applied to MCP text output (sanitization, quarantine, trust-level gating), plus new
binary-specific controls (format/size/dimension validation, ephemeral-only lifetime, redacted
`Debug`).

### Out of Scope (v1)

- **Audio** `ContentBlock::Audio` passthrough — no `MessagePart::Audio` variant exists; adding
  one is an **Ask First** decision under invariant #4 (`001-system-invariants/spec.md` §"Ask
  First" — "Adding a new `MessagePart` variant"). Deferred to a future spec.
- Embedded blob `ContentBlock::Resource`/`ResourceLink` passthrough — same reasoning; today's
  text placeholder is retained.
- Per-model, format-aware vision capability tables — v1 accepts the existing coarse
  `LlmProvider::supports_vision() -> bool`, documented as a known limitation (§7).
- `MessagePart::ToolResult`/`ToolUse` structural extension to natively nest image content
  inside the tool-result block (Option B in the architect's alternatives) — rejected for v1 as
  an Ask-First `MessagePart` contract change; the sibling-part design (§3) avoids it entirely.
- Persisting/rehydrating images across compaction, session resume, or export — all `Image`
  parts (MCP-sourced and user-upload) are ephemeral, current-turn-only (§4, C1).

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN an MCP server has `media_passthrough = true` in its config AND is not `McpTrustLevel::Sandboxed` AND a tool result contains one or more `ContentBlock::Image` blocks THE SYSTEM SHALL decode and validate them via `MediaSanitizer` before the tool result is classified | must |
| FR-002 | WHEN `media_passthrough` is unset, `false`, or the server is `Sandboxed` THE SYSTEM SHALL behave exactly as today (text placeholder only, no decode attempt) | must |
| FR-003 | WHEN a validated image is available for a successful tool result AND the provider selected for the next request call is vision-capable THE SYSTEM SHALL attach the image as a sibling `MessagePart::Image` in the same `Role::User` message as the corresponding `MessagePart::ToolResult` | must |
| FR-004 | WHEN a validated image is available but the provider (or, for routed/cascade requests, the concretely selected tier) is not vision-capable THE SYSTEM SHALL drop the image and rely on the existing text placeholder — NEVER send a 400/422-triggering request | must |
| FR-005 | WHEN an image fails `MediaSanitizer` validation (bad magic bytes, disallowed format, oversized, over-dimension, over per-turn budget) THE SYSTEM SHALL drop that image, keep the text placeholder, and log the rejection reason via the tool audit path | must |
| FR-006 | WHEN a tool result is an error or partial result THE SYSTEM SHALL NOT emit an `Image` part for it, regardless of `media_passthrough` — only `process_one_tool_result`'s success path may attach media | must |
| FR-007 | WHEN a `MessagePart::ToolResult`'s companion text is quarantined by the existing sanitizer quarantine flow THE SYSTEM SHALL NOT emit that result's `Image` sibling | must |
| FR-008 | WHEN any message is passed to `serialize_parts_json` (SQLite persist), the Qdrant embed-text extraction path, or `SessionSink::record_message`/`record_user_message` (durable JSONL log) THE SYSTEM SHALL exclude all `MessagePart::Image` parts — persistence is scoped to the durable projection paths listed here (memory-window pruning during a live turn is out of scope; see §4 C1) | must |
| FR-009 | WHEN `zeph-config --migrate-config` runs on a pre-072 config THE SYSTEM SHALL add `media_passthrough = false` to every existing `[[mcp.servers]]` entry and add default `[mcp.media]` values | must |
| FR-010 | WHEN `--init` runs the MCP server wizard step THE SYSTEM SHALL prompt for media passthrough per server, defaulting to `No` | should |
| FR-011 | WHEN media passthrough is enabled for at least one configured server in the session THE SYSTEM SHALL add one static system-prompt line at session/config-assembly time (never per-turn) warning that MCP-sourced images are untrusted content | must |
| FR-012 | WHEN `ImageData` or any type composing it is formatted via `{:?}` THE SYSTEM SHALL render `[image: {mime_type}, {n} bytes]` and never the raw byte payload | must |

---

## 3. Architecture

### 3.1 Corrected integration point (deviation from the architect's plan — see §9)

The architect's plan (handoff `2026-07-13T18-13-11-architect.md`) and both critic passes cite
`process_successful_tool_output` (`tool_result.rs:648`) and `MessagePart::ToolOutput` as the
emission hook and part variant. **Independent re-verification for this spec found both citations
describe dead code**: `process_successful_tool_output` and its caller `handle_tool_result`
(`tool_result.rs:541-620`) are `#[cfg(test)]`-gated — they do not exist in a production build.
The real, always-compiled production path is:

```
process_tool_result_batch (tier_loop.rs:2478)      — batch orchestrator, one call per LLM turn
  └─ for each tool call:
       process_one_tool_result (tool_result.rs:369) — per-result classify → sanitize → push
            classify_tool_result (tool_result.rs:266) — Result<Option<ToolOutput>, ToolError>
                                                          → ToolResultClassification
            result_parts.push(MessagePart::ToolResult { tool_use_id, content, is_error })
                                                        (tool_result.rs:471-475)
  └─ Message::from_parts(Role::User, result_parts)   (tier_loop.rs:2562)
  └─ persist_message(...)                            (tier_loop.rs:2572)
  └─ push_message(user_msg)                           (tier_loop.rs:2580)
```

The correct emission point is therefore **inside `process_one_tool_result`, immediately after
the `result_parts.push(MessagePart::ToolResult{..})` at `tool_result.rs:475`** — pushing zero or
one additional `MessagePart::Image` per tool result into the same `result_parts` vector that
`process_tool_result_batch` later wraps into one `Role::User` message. `MessagePart::ToolResult`
is the part variant carried in production, not `MessagePart::ToolOutput` (`ToolOutput` is a
different variant used for a separate, non-tool-loop-batch code path — see `provider.rs:279`).

**Plumbing gap this reveals:** `classify_tool_result` (`tool_result.rs:266-360`) unpacks
`zeph_tools::ToolOutput` into `ToolResultClassification` (`tool_execution/mod.rs:88-98`) and
copies out only `output` (renamed from `out.summary`), `diff`, `inline_stats`, `kept_lines`,
`locations` — it does **not** carry forward a hypothetical `out.media` field. `ToolResultClassification`
must gain a `media: Vec<zeph_llm::ImageData>` field, populated **only** in the `Ok(Some(out))` arm
(`:289-298`) from `out.media`, and left empty in the `Ok(None)` (`:301-309`) and `Err` (`:346-360`)
arms. This mechanically satisfies FR-006 (error/partial results never carry media) by
construction, not by a separate check.

Claude's request builder already treats `MessagePart::Image` as a "structured part" alongside
`MessagePart::ToolResult` within the same `Role::User` message (`claude/request.rs:111-121`) —
appending an `Image` part to `result_parts` requires no provider-side changes; it becomes one
more content block in the same user turn, not nested inside any specific `tool_result` block
(that nesting is the deferred Option B).

### 3.2 Data flow (v1)

1. `McpToolExecutor::execute_tool_call` (`zeph-mcp/src/executor.rs:96-137`) still calls
   `render_content_blocks` unconditionally — the text placeholder is always present. When the
   owning server has `media_passthrough = true` (and is not `Sandboxed`), it additionally
   iterates `result.content` for `ContentBlock::Image` blocks and passes each through
   `MediaSanitizer::sanitize_image`, collecting successes into `ToolOutput.media`.
2. `ToolOutput.media: Vec<zeph_llm::ImageData>` (new field, default empty) carries validated
   bytes across the tool boundary to `zeph-core`.
3. `classify_tool_result` copies `out.media` into `ToolResultClassification.media` (success path
   only, per §3.1).
4. `process_one_tool_result`, after building and sanitizing the text `MessagePart::ToolResult`
   (unchanged), appends `MessagePart::Image` sibling parts to `result_parts` **iff**: not
   `is_error`, not `vigil_blocked` (quarantine, FR-007), and the "requires-vision" routing check
   (§3.3, FR-003/FR-004) resolves to a vision-capable target — otherwise the media is dropped
   with a `tracing::warn!` (the text placeholder already informs the model).
5. `process_tool_result_batch` builds `Message::from_parts(Role::User, result_parts)` exactly as
   today; the vector now may contain trailing `Image` parts after the batch's `ToolResult` parts.
6. `Agent::persist_message` (`agent/persistence/store.rs:24`) strips all `Image` parts from
   `parts` **before** calling `sink.record_message` and **before** `svc.persist_message` (§4, C1) —
   the in-memory `Message` pushed via `push_message` (step above) keeps its `Image` parts for the
   current turn's provider request only.

### 3.3 Vision-tier routing (S3)

`LlmProvider::supports_vision()` on a `Router`/`Triage` provider aggregates via
`.any(...)` (`router/triage.rs:641-643`) — true if *any* tier supports vision, even if the tier
actually selected for the next call does not. Attaching an `Image` part based on the aggregate
alone can produce a request to a text-only tier that cannot encode it (400/422).

**Binding behavior:** when `result_parts` contains a to-be-attached `Image` part, the turn's
provider-selection step must resolve to one of:
- **(a)** a concretely vision-capable tier is selected for the immediately following
  `chat_with_tools` call for this turn, and the `Image` part is kept; or
- **(b)** no vision-capable tier can be guaranteed for the call, and the `Image` part(s) are
  dropped before the request is built, leaving only the already-present text placeholder.

In no case may an `Image` part be sent to a provider that returns `supports_vision() == false`
for its own account. For a non-router single provider, the existing per-provider
`supports_vision()` gate (as used in `build_user_message`, `agent/mod.rs:1740`) is correct
as-is, since the aggregate equals the concrete provider. Whether forcing a vision-capable tier
overrides cascade cost-ordering is left to the developer's routing-strategy implementation; the
only pinned, testable rule is: **a turn carrying an unresolved-vision Image part never reaches
the provider as a 400/422** (Acceptance Criterion AC-6).

### 3.4 Key Types

- **`ToolOutput.media: Vec<zeph_llm::ImageData>`** — new field on the existing
  `zeph-tools::executor::ToolOutput` struct (`executor.rs:267-`), default empty. `zeph-tools`
  gains one new dependency edge on `zeph-llm` (confirmed no cycle both directions;
  `zeph-sanitizer` already depends on `zeph-llm`, `Cargo.toml:26`). No mirror/newtype — a single
  `ImageData` type flows `zeph-mcp` → `zeph-tools` → `zeph-core` → `zeph-llm` unchanged.
- **`ToolOutput` gains `#[derive(Default)]`.** `ToolName: Default` is compiler-proven
  (`ToolCall` at `executor.rs:49` derives `Default` with a `tool_id: ToolName` field and
  compiles today); every other `ToolOutput` field is already `Default`-able
  (`String`/`u32`/`Option<_>`/`bool`). All **271** `ToolOutput { .. }` struct-literal
  construction sites (across ~86 files, ~185 non-test) are migrated to add `..Default::default()`
  in the same PR that adds the `media` field — a one-time mechanical edit, not "avoided" by any
  builder (see §9 for why the original `with_media` builder claim was dropped).
- **`ToolResultClassification.media: Vec<zeph_llm::ImageData>`** (new field, `tool_execution/mod.rs:88-98`)
  — see §3.1 plumbing gap.
- **`MediaSanitizer`** (new, `zeph-sanitizer`) —
  `fn sanitize_image(&self, bytes: &[u8], declared_mime: &str, server_id: &str) -> Result<zeph_llm::ImageData, MediaRejected>`.
  Decodes via the `image` crate (already present transitively in `Cargo.lock` at `0.25.10`
  pulled in with only `png`+`tiff` decoder features enabled — **not currently a direct workspace
  dependency of any crate**; this spec adds it as a direct dependency of `zeph-sanitizer` with
  `png`, `jpeg`, `gif`, `webp` features explicitly enabled) on `spawn_blocking`. Enforces, in
  order: (a) magic-byte sniff against the declared MIME (closes the `detect_image_mime`
  unknown→`image/png` gap, `message_queue.rs:36`), (b) format allowlist (JPEG/PNG/GIF/WebP),
  (c) per-image byte cap, (d) max-dimension/max-pixel cap at decode time (decompression-bomb
  defense — a byte cap alone cannot bound decoded pixel count), (e) per-tool-result image count
  cap, (f) per-turn image budget (aggregated across the whole `execute_tool_calls_batch`, not
  per-tool). Trust level is always `ExternalUntrusted` for MCP-sourced images; no re-encode/strip
  step is required for v1 beyond the decode-and-recap (metadata-strip is a natural byproduct of
  re-encoding through `image`, not a separate requirement).
- **`McpServerConfig.media_passthrough: bool`** (new field, default `false`) — per-server
  opt-in, independent axis from `McpTrustLevel` but still hard-blocked when
  `trust_level == Sandboxed` regardless of the flag.
- **`McpMediaConfig`** (new, under global `[mcp.media]` in `McpConfig`, `channels.rs:1209-`) —
  `max_image_bytes` (default 5 MiB — below the existing 20 MiB user-upload
  `MAX_IMAGE_BYTES`, `message_queue.rs:14`), `max_dimension_px` (default 8192), `max_pixels`
  (default 64_000_000 ≈ 64 MP), `max_images_per_result` (default 4), `max_images_per_turn`
  (default 8), `allowed_formats` (default `["jpeg", "png", "gif", "webp"]`).

### 3.5 Custom `Debug` on `ImageData` (S2)

Replace the derived `Debug` on `zeph_llm::provider::ImageData` (`provider.rs:343`, currently
derives over `data: Vec<u8>`) with a hand-written `impl Debug` rendering
`[image: {mime_type}, {n} bytes]`. `ToolOutput` and `MessagePart::Image` both compose
`ImageData` and derive their own `Debug` — the redaction is inherited automatically; no manual
`Debug` impl is needed on either wrapper. This also closes a pre-existing leak on the
user-upload image path (project has 9+ prior Debug-derive content-leak incidents).

---

## 4. Key Invariants

### C1 — Ephemeral media: single, explicit strip point above all persistence surfaces (M5-refined)

**There are three persistence/embed surfaces, not two:** (1) SQLite `parts_json`
(`serialize_parts_json`, `embed.rs:122`), (2) the Qdrant embed-text extraction path, and (3) the
durable JSONL session-event log (`SessionSink::record_message`, `session_sink.rs`), which
dual-writes and — per its own doc comment — runs **before** `PersistenceService::persist_message`.

**Binding placement:** the strip happens once, in `Agent::persist_message`
(`crates/zeph-core/src/agent/persistence/store.rs:24`), on the `parts` slice, **before** it is
passed to either `sink.record_message` (currently `store.rs:57`) or `svc.persist_message`
(currently `store.rs:89`) — not downstream inside `zeph-agent-persistence` alone, which would
leave the JSONL log covered only by `record_user_message`'s current accidental allowlist
behavior (it only ever serializes `MessagePart::ToolResult`, silently `continue`-ing past
`Image` — a structural accident, not an enforced control). `SessionEvent::UserMessage` already
carries an unused `image_refs: Vec<_>` field; a future change populating it would silently
reintroduce the leak past a downstream-only strip, which is why the strip must sit above the
fan-out to both writers.

**Scope:** strip **all** `MessagePart::Image` parts from persistence/embed, not only
MCP-sourced ones. `hydrate.rs` (`:285,338,359,420`) already reconstructs only
`Text`/`ToolUse`/`ToolResult` on rehydrate — persisting any `Image` today is already dead weight
(base64 written, then silently dropped on hydrate). Making persist consistent with hydrate is
strictly better and avoids needing per-image provenance tagging to decide what to strip.

**In-memory scope (not persistence):** the live `Message.parts` pushed via `push_message`
(`tier_loop.rs:2580`) keeps its `Image` parts for the *current* turn's provider request only.
Compaction/summarization operate on persisted/text parts; since `Image` parts never reach
persistence, compaction never encounters one there. Mid-turn (pre-persist), an in-flight `Image`
part is non-summarizable and passes through untouched until the strip point.

### C2 — Default posture is opt-in, per-server, hard-blocked for Sandboxed

`media_passthrough` defaults to `false`. Enabling it never overrides
`McpTrustLevel::Sandboxed` — a Sandboxed server never gets media passthrough regardless of the
flag. Aligns with the "only demote, never elevate" restriction-level rule already used elsewhere
in `zeph-mcp` trust handling.

### C3 — Vision-capable-tier gating never produces a runtime 400/422

Per §3.3: when a router/cascade cannot guarantee the concretely selected tier is vision-capable
for the pending request, the `Image` part(s) are dropped before the request is built. The text
placeholder is the guaranteed fallback in every case. This is the LLM Serialization Gate concern
(`.claude/rules/continuous-improvement.md`) and requires a live cascade + MCP-image session test
before merge (§6, AC-6).

### C4 — Redacted `Debug`, never raw bytes in logs/dumps

Per §3.5 and FR-012.

### C5 — Pre-assembly passes must remain `Image`-part-safe (M6, critic-hardening)

The corrected emission point (§3.1, `tool_result.rs:475`) pushes `MessagePart::Image` siblings
into `result_parts` **before** three pre-assembly passes that also run over `result_parts` inside
`process_tool_result_batch`, in this order: `run_causal_ipi_post_probe` (`tier_loop.rs:2553`),
`record_shadow_event` (`:2557`), and `apply_acon_compression` (`:2559-2560`).

**Verified safe today, by construction, not by design:** `apply_acon_compression` filters to
`MessagePart::ToolResult` and maps entries by `tool_use_id`; `run_causal_ipi_post_probe`
pattern-matches `if let MessagePart::ToolResult { .. }`; `record_shadow_event` takes
`tool_calls`, not `result_parts`, as its input. None of the three currently touches, reorders, or
drops a `MessagePart::Image` sibling. This is the same class of implicit-structural-safety
assumption that caused the original miscitation of the emission hook (§9) — it happens to hold
today but is not an enforced contract.

**Binding invariant:** `run_causal_ipi_post_probe`, `record_shadow_event`, and
`apply_acon_compression` (and any future pass inserted between the `MessagePart::Image` push at
`tool_result.rs:475` and `Message::from_parts` at `tier_loop.rs:2562`) MUST treat any
`MessagePart` variant other than `ToolResult`/`ToolUse` as opaque and pass it through unmodified
— never drop, reorder relative to its preceding `ToolResult`, or mutate a `MessagePart::Image`
(or any other non-`ToolResult` variant). A future refactor of any of these three passes that
adds `Image`-touching logic without updating this invariant is a spec violation, not a free
extension point.

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| MCP server returns an image but `media_passthrough` is unset/false | Text placeholder only (today's behavior), no decode attempted (FR-002) |
| MCP server is `Sandboxed` with `media_passthrough = true` | Flag is ignored; text placeholder only (C2) |
| Image fails magic-byte sniff (declared MIME mismatches actual bytes) | Dropped, logged via tool audit, text placeholder remains (FR-005) |
| Image exceeds `max_image_bytes`, `max_dimension_px`, or `max_pixels` | Dropped before or during decode (`spawn_blocking`), never fully decoded into memory if the byte cap alone catches it first (FR-005) |
| Tool result contains > `max_images_per_result` images | Only the first N (config cap) are validated/attached; remainder dropped with a log line noting the truncation |
| Turn's cumulative attached images exceed `max_images_per_turn` across a batch of tool calls | Aggregate cap enforced across `execute_tool_calls_batch`; excess dropped, images already accepted for earlier tool results in the batch are kept |
| Tool result is an error (`is_error = true`) or a partial/`Ok(None)` result | No `Image` part ever emitted — `classify_tool_result`'s `Ok(None)`/`Err` arms never populate `ToolResultClassification.media` (FR-006, by construction) |
| Text companion is quarantined by the sanitizer's existing quarantine flow (`vigil_blocked`) | The `Image` sibling for that same tool result is not emitted — an image cannot be fact-extracted the way a quarantine summarizer processes text, so dropping it preserves the capability-reduction guarantee (FR-007) |
| Selected provider/tier for the pending request is not vision-capable | `Image` part(s) dropped before the request is built; text placeholder is the fallback; never a 400/422 (C3, FR-004) |
| Batch of N tool calls, only some return images | Each tool result's `Image` sibling (if any) is independent; caps are per-result and per-turn-aggregate as above |
| Media-enabled session assembling the system prompt | One static caveat line added once at session/config-assembly time — never per-turn, so the prompt-cache prefix is undisturbed (FR-011) |
| `--migrate-config` run against a config with existing `[[mcp.servers]]` entries | Every entry gains `media_passthrough = false`; `[mcp.media]` gains full defaults (FR-009) |
| Debug-dump or `tracing::debug!(?tool_output)` on a value containing `ImageData` | Renders `[image: {mime}, {n} bytes]`, never raw bytes (FR-012) |

---

## 6. Success Criteria (Acceptance Criteria)

All criteria are observable and testable.

| ID | Criterion | How to verify |
|----|-----------|---------------|
| AC-1 | Default posture: with `media_passthrough` unset, an MCP image tool result never attaches an `Image` part | Integration test: call a mock MCP server returning `ContentBlock::Image`; assert no `MessagePart::Image` reaches the built `Role::User` message |
| AC-2 | Opt-in end-to-end: with `media_passthrough = true` on an `Untrusted`-or-`Trusted` server and a vision-capable provider, a valid PNG/JPEG/GIF/WebP tool-result image is attached as `MessagePart::Image` in the same turn | Integration test against a mock provider asserting the parts vector |
| AC-3 | `Sandboxed` override: `media_passthrough = true` on a `Sandboxed` server never attaches media | Config-level unit test |
| AC-4 | Validation rejects: a magic-byte mismatch, an oversized file, and an over-dimension image are each rejected with the text placeholder retained and a logged reason | Unit tests per rejection class, one for each of byte-cap / dimension-cap / format-mismatch |
| AC-5 | Persistence exclusion (C1): after a full tool-result round-trip with an attached `Image` part, the message is **not** present in SQLite `parts_json`, **not** present in any Qdrant payload/vector, and **not** present in the durable session JSONL log | Integration test asserting all three surfaces post-turn |
| AC-6 | Vision-tier routing (C3): a cascade pool `[text-only cheap tier, vision-capable quality tier]` handling a turn with an attached `Image` part either routes to the vision tier or drops the image before the request — in no run does the provider return 400/422 | Live cascade + MCP-image session test (mandatory pre-merge per LLM Serialization Gate) plus an automated regression test simulating the tier-selection seam |
| AC-7 | Error/partial results never carry media (FR-006) | Unit test: an `Err(ToolError::..)` and an `Ok(None)` tool result both produce empty `ToolResultClassification.media` regardless of what `ToolOutput.media` would have contained |
| AC-8 | Quarantine suppression (FR-007) | Unit test: a tool result whose text companion triggers `VigilOutcome::Blocked` does not emit its `Image` sibling |
| AC-9 | Redacted `Debug` (FR-012) | Unit test: `format!("{:?}", image_data)` and `format!("{:?}", tool_output_with_media)` both exclude any base64/byte-array representation |
| AC-10 | `--migrate-config` idempotency (FR-009) | Run migration twice on a fixture config; second run is a no-op, `media_passthrough` present on every server entry |
| AC-11 | `--init` wizard prompts for media passthrough per server, default No (FR-010) | Wizard integration/golden test |
| AC-12 | Static system-prompt caveat is cache-safe (FR-011) | Test asserting the caveat line is identical (byte-for-byte) across two consecutive turns of the same session when passthrough is enabled — i.e., it is assembled once, not re-derived per turn |
| AC-13 | Count/budget caps (per-result and per-turn) enforced | Unit tests: a result with `max_images_per_result + 1` images attaches only the cap; a batch whose combined images exceed `max_images_per_turn` attaches only up to the cap |
| AC-14 | Audit trail | Every accept/reject decision (server, tool, mime, byte count, outcome) appears in the existing tool audit log |
| AC-15 | Pre-assembly pass safety (C5): with an interleaved `MessagePart::Image` sibling present in `result_parts` alongside multiple `ToolResult` parts, `apply_acon_compression` still correctly targets the intended `ToolResult` by `tool_use_id` (unaffected by the presence/position of the `Image` sibling), and the `Image` part reaches the assembled `Message` byte-for-byte unmodified (same `mime_type`, same `data`) after all three pre-assembly passes (`run_causal_ipi_post_probe`, `record_shadow_event`, `apply_acon_compression`) have run | Regression test: build a batch of ≥2 tool calls where one result carries an `Image` sibling positioned between two `ToolResult` parts; run the full `process_tool_result_batch` path; assert (a) acon compression output for the non-adjacent `ToolResult` is unchanged from a control run without the `Image` sibling, and (b) the `Image` part in the final `Message.parts` is `==` the pre-assembly value |

---

## 7. Multi-Model Design Compliance

Gating reuses the existing `LlmProvider::supports_vision() -> bool` (`provider.rs:843`) — no
hardcoded provider or model name is introduced. Known, accepted v1 limitation: `supports_vision()`
is a per-provider-instance boolean, not model- or MIME-aware (e.g., Ollama returns `true` even
for a text-only local model). §3.3 defines the router-aggregation fix (S3) that makes this
limitation safe (never a 400/422) without requiring a per-model capability table. A per-model
vision-capability table is a noted future extension:

<!-- TODO(#5366): per-model vision-capability table replaces coarse supports_vision() boolean; see specs/072 §7 -->

---

## 8. Threat Model (mandatory)

**New attack surface:** an untrusted MCP server returns an image that a vision model reads
directly, bypassing every text-injection defense (intent-anchor nonce, spotlight, quarantine,
NLI/classifier) that only operates on text. Documented attack classes: steganographic/
embedded-text prompt injection (instructions rendered in pixels or hidden in metadata), and the
**sleeper-channel** pattern (untrusted bytes persisted then re-fired through a different surface
— compaction, subagent, later turn — where provenance is lost).

Controls (all binding, see §4 for the corresponding invariants):

1. **Opt-in, default OFF, per server** (C2). Never auto-enabled; never overridden by
   `Sandboxed`.
2. **Text companion always present.** The `[image: mime, N bytes]` placeholder remains in the
   sanitized, anchor-wrapped tool-result text regardless of whether the image itself is
   attached.
3. **Binary validation** (`MediaSanitizer`, §3.4): magic-byte sniff, format allowlist,
   declared-vs-actual MIME check, byte cap, dimension/pixel cap (decompression-bomb defense),
   per-result and per-turn count caps.
4. **Ephemeral, never persisted** (C1). MCP-sourced (and all) `Image` parts never reach SQLite,
   Qdrant, or the durable JSONL log — no sleeper-channel re-entry via hydrate, compaction, or
   session resume.
5. **Never silently sent to an incapable provider** (C3). A routing mismatch degrades to the
   text placeholder, never a runtime error.
6. **Capability gate** = `supports_vision()`, resolved at the concrete-tier level (§3.3), not
   the router aggregate.
7. **Audit.** Every passthrough decision is logged via the existing tool audit path (AC-14).
8. **Redacted `Debug`** (C4) prevents raw bytes leaking into logs/dumps — a provenance-laundering
   vector distinct from the six controls above (a log file is a "trusted" surface an attacker
   could otherwise use to smuggle bytes past the ephemeral-persistence control).

Residual risk, documented and accepted for v1: pixel-level LSB steganography is not defeated by
metadata strip/re-encode (re-encoding through `image` does strip EXIF/metadata channels as a
byproduct, but not pixel-domain steganography). This is the same class of residual risk any
vision-capable system accepts; no v1 mitigation is proposed beyond the controls above.

---

## 9. Deviation From Prior Plan (traceability)

This spec is derived from the architect's REVISION 1 plan (handoff
`2026-07-13T18-13-11-architect.md`) and two critic passes (`2026-07-13T18-23-14-critic.md`,
verdict `significant`; `2026-07-13T18-32-03-critic.md`, verdict `minor`, approved). All
architect/critic resolutions (C1 persistence scope, S1 `Default` migration, S2 redacted `Debug`,
S3 vision-tier routing, S4 single `ImageData` type, M1-M4, M5 strip placement) are carried
forward into this spec unchanged **except** one correction found during spec-authoring
verification:

- **The emission hook and `MessagePart` variant were incorrect in the prior plan.**
  `process_successful_tool_output`/`MessagePart::ToolOutput` (as cited by the architect and
  accepted by both critic passes without independent re-derivation of the call graph) are
  `#[cfg(test)]`-gated dead code in production. The real hook is `process_one_tool_result`
  building `MessagePart::ToolResult` inside `process_tool_result_batch` (§3.1). This does not
  change any of the architect's resolved design decisions (C1/S1-S4/M1-M5) — it changes *where*
  in the code they are implemented. `ToolResultClassification` (not previously in scope) gains a
  `media` field as a consequence (§3.1).
- The `image`-crate open question (M1) is resolved as **present transitively at 0.25.10 with
  only `png`+`tiff` features enabled, not a direct workspace dependency of any crate** — this
  spec adds it as a direct `zeph-sanitizer` dependency with `jpeg`/`gif`/`webp` features
  additionally enabled (§3.4).
- Concrete cap defaults (previously an open question) are pinned in §3.4:
  `max_image_bytes = 5 MiB`, `max_dimension_px = 8192`, `max_pixels ≈ 64 MP`,
  `max_images_per_result = 4`, `max_images_per_turn = 8`. These are conservative starting
  defaults, tunable via `[mcp.media]`; a follow-up benchmarking pass may adjust them (§10, open
  question).

**Critic re-review (handoff, verdict `minor`) confirmed the correction above and raised one
additional non-blocking hardening item, folded into this spec as C5/AC-15:**

- **M6 — pre-assembly pass safety.** The corrected emission point (`tool_result.rs:475`) places
  `MessagePart::Image` siblings into `result_parts` before three passes inside
  `process_tool_result_batch` that also operate on `result_parts`:
  `run_causal_ipi_post_probe` (`:2553`), `record_shadow_event` (`:2557`), and
  `apply_acon_compression` (`:2559-2560`). All three are verified Image-safe **today**, by
  construction (acon filters to `ToolResult` and maps by `tool_use_id`; the causal-IPI probe
  pattern-matches `ToolResult`; shadow-event takes `tool_calls`, not `result_parts`) — this is not
  a bug, but it is the same class of implicit-structural-safety assumption that produced the
  original miscitation, so it is now an explicit, testable invariant (§4 C5) rather than an
  unstated accident.

---

## 10. Open Questions

| ID | Question | Status |
|----|----------|--------|
| OQ-1 | Are the pinned cap defaults (5 MiB / 8192px / 64MP / 4 per result / 8 per turn) right for real-world MCP image tools (screenshot tools, chart renderers)? | Deferred to a post-implementation benchmarking pass; defaults are conservative and configurable |
| OQ-2 | Should `max_images_per_turn` interact with the existing 20 MiB user-upload `MAX_IMAGE_BYTES` as a combined per-turn byte budget, or remain fully independent? | v1: fully independent (MCP media budget is separate from user-upload budget); revisit if real usage shows contention |

---

## 11. Affected Subsystems

| Crate | Change level | What changes |
|-------|-------------|--------------|
| `zeph-mcp` | Medium | Decode `ContentBlock::Image` in `execute_tool_call` when server opts in; populate `ToolOutput.media` |
| `zeph-tools` | Small (+ mechanical) | `ToolOutput.media` field + `#[derive(Default)]` + 271-site `..Default::default()` migration; new `zeph-llm` dependency edge |
| `zeph-sanitizer` | Medium | New `MediaSanitizer`; new direct `image` crate dependency (jpeg/png/gif/webp features) |
| `zeph-config` | Small | `McpServerConfig.media_passthrough`; `McpConfig.media: McpMediaConfig`; `--init` wizard step; `--migrate-config` step |
| `zeph-core` | Medium | `ToolResultClassification.media`; `process_one_tool_result` sibling-Image emission gated per §3.3; `Agent::persist_message` strip point (C1); static system-prompt caveat assembly |
| `zeph-agent-persistence` | Small | No code change beyond what `persist_message`'s pre-stripped `parts` already guarantees — `serialize_parts_json`/embed/`SessionSink` receive Image-free slices by construction; add the C1 integration test here |
| `zeph-llm` | Small | Custom `impl Debug` for `ImageData` |

---

## 12. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide non-negotiable principles
- [[001-system-invariants/spec]] — Invariant #4 (Ask First: new `MessagePart` variant, engaged for the Audio deferral), #5 (`ToolExecutor`/`ToolOutput` contract), #12 (mandatory integration points)
- [[008-3-security]] — MCP elicitation/injection defense this spec's threat model extends
- [[010-2-injection-defense]] — Text-sanitization pipeline the text placeholder continues to flow through unchanged
- [[040-content-sanitizer]] — `ContentSanitizer`/quarantine flow `MediaSanitizer` sits alongside
- [[069-threat-model/spec]] — MATRA asset/attack-tree model; this spec should add an MCP-media asset/attack-tree entry as a follow-up
- [[068-session-persistence/spec]] — `SessionSink`/durable JSONL log this spec's C1 strip point must precede
- `plan.md` — phased implementation plan
- `tasks.md` — concrete task breakdown
