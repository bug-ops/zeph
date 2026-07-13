---
aliases:
  - Multimodal MCP Passthrough Tasks
  - Tasks 072
tags:
  - tasks
  - mcp
  - llm
  - security
created: 2026-07-13
status: draft
related:
  - "[[072-multimodal-mcp-passthrough/plan]]"
  - "[[072-multimodal-mcp-passthrough/spec]]"
---

# Implementation Tasks 072 — Multimodal MCP `ContentBlock` Passthrough

Tasks are ordered by phase and dependency. Each task has: ID, phase, crate owner, description,
and spec references.

---

## Phase P0 — Type Plumbing

### T-001 — Add `zeph-llm` dependency to `zeph-tools`
**Owner:** rust-developer
**Crate:** `zeph-tools`
**Spec refs:** §3.4 (Key Types), §9 (S4)
Add `zeph-llm.workspace = true` to `crates/zeph-tools/Cargo.toml`. Verify with
`cargo tree -p zeph-tools` that no cycle is introduced and `cargo tree -p zeph-llm` does not
reach `zeph-tools`.

### T-002 — Add `ToolOutput.media` field and `#[derive(Default)]`
**Owner:** rust-developer
**Crate:** `zeph-tools`
**Spec refs:** §3.4
In `crates/zeph-tools/src/executor.rs`, add `#[derive(Default)]` to `ToolOutput` (`:267`) and a
new field `pub media: Vec<zeph_llm::ImageData>` with a doc comment explaining it carries
validated MCP-sourced (or future) image data across the tool boundary, empty for all executors
that don't produce media. Depends on: T-001.

### T-003 — Migrate all `ToolOutput { .. }` struct literals to `..Default::default()`
**Owner:** rust-developer
**Crate:** workspace-wide (~86 files)
**Spec refs:** §3.4, §9 (S1)
Mechanical edit: every `ToolOutput { .. }` construction site (271 literals, confirmed via
`rg 'ToolOutput\s*\{'`) gets `..Default::default()` appended so the new `media` field defaults
to empty without touching unrelated fields. Run
`cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`
after to catch any missed site (a literal without `..Default::default()` and without an explicit
`media:` field will fail to compile — this is the intended forcing function). Depends on: T-002.

### T-004 — Custom redacting `Debug` for `ImageData`
**Owner:** rust-developer
**Crate:** `zeph-llm`
**Spec refs:** §3.5, §9 (S2)
In `crates/zeph-llm/src/provider.rs`, remove `Debug` from `ImageData`'s derive list (`:343`) and
add a hand-written `impl std::fmt::Debug for ImageData` rendering
`[image: {mime_type}, {n} bytes]`. Unit test: `format!("{:?}", ...)` contains no byte-array
representation. This also fixes the pre-existing user-upload image leak — no separate task
needed for that.

### T-005 — Add `media` field to `ToolResultClassification`
**Owner:** rust-developer
**Crate:** `zeph-core`
**Spec refs:** §3.1
In `crates/zeph-core/src/agent/tool_execution/mod.rs`, add
`media: Vec<zeph_llm::ImageData>` to `ToolResultClassification` (`:88-98`).

### T-006 — Thread `media` through `classify_tool_result`
**Owner:** rust-developer
**Crate:** `zeph-core`
**Spec refs:** §3.1, §5 (edge case table)
In `crates/zeph-core/src/agent/tool_execution/tool_result.rs`, `classify_tool_result`
(`:266-360`): `Ok(Some(out))` arm (`:289-298`) sets `media: out.media`; `Ok(None)` arm
(`:301-309`) and `Err` arm (`:346-360`) both set `media: Vec::new()`. Tests:
`test_classify_tool_result_error_arm_media_empty`, `test_classify_tool_result_none_arm_media_empty`.
Depends on: T-002, T-005.

### T-007 — P0 regression pass
**Owner:** rust-agents:rust-testing-engineer
**Crate:** workspace-wide
**Spec refs:** plan.md P0 Acceptance Criteria
Full `cargo nextest run --workspace --features "desktop,ide,server,chat,pdf,scheduler,testing" --lib --bins`
after T-001..T-006. No existing test's assertions on non-`media` `ToolOutput` fields should
change. `cargo doc` gate must pass with the new doc comments.

---

## Phase P1 — Ephemeral Persistence Strip

### T-101 — Strip `Image` parts before both persistence writers in `persist_message`
**Owner:** rust-developer
**Crate:** `zeph-core`
**Spec refs:** §4 (C1)
In `crates/zeph-core/src/agent/persistence/store.rs`, `Agent::persist_message` (`:24-`): compute
`let persisted_parts: Vec<MessagePart> = parts.iter().filter(|p| !matches!(p, MessagePart::Image(_))).cloned().collect();`
(or an equivalent slice-filtering approach avoiding unnecessary clones where possible) before the
existing `if let Some(sink) = ...` block (`:55`). Pass `&persisted_parts` to
`sink.record_message(role, content, &persisted_parts)` (was `:57`, using `parts`) and to
`PersistMessageRequest::from_borrowed(role, content, &persisted_parts, has_injection_flags)`
(was `:63`, using `parts`). Add a doc comment on `persist_message` citing spec-072 §4 C1
explaining the strip is deliberate.

### T-102 — Persistence-exclusion tests (SQLite, Qdrant, JSONL)
**Owner:** rust-agents:rust-testing-engineer
**Crate:** `zeph-core`, `zeph-agent-persistence`
**Spec refs:** AC-5
Three tests: `test_persist_message_strips_image_before_sqlite`,
`test_persist_message_strips_image_before_embed`,
`test_persist_message_strips_image_before_session_log`. Each constructs a `Message`/parts slice
containing a `MessagePart::Image` sibling, calls `persist_message`, and asserts the respective
surface (SQLite `parts_json` string, embed-text input, `SessionEventLog` JSONL output) contains
no image-kind content. Depends on: T-101.

### T-103 — In-memory retention regression test
**Owner:** rust-agents:rust-testing-engineer
**Crate:** `zeph-core`
**Spec refs:** §4 (C1, in-memory scope)
`test_persist_message_inmemory_message_keeps_image` — proves the strip is scoped to the two
persistence-writer calls only; the caller's own `Message` object (pushed via `push_message`
separately) is unaffected and still carries its `Image` part for the current turn's provider
request. Depends on: T-101.

---

## Phase P2 — Decode, Validate, Attach

### T-201 — Add `image` crate dependency to `zeph-sanitizer`
**Owner:** rust-developer
**Crate:** `zeph-sanitizer`
**Spec refs:** §3.4, §9
Add `image = { version = "0.25", default-features = false, features = ["jpeg", "png", "gif", "webp"] }`
to `crates/zeph-sanitizer/Cargo.toml`. Confirm via `cargo tree` the version resolves to the
already-locked `0.25.10` (no unexpected major bump) or update `Cargo.lock` deliberately if a
newer patch is pulled in — check current versions via context7 mcp per project dependency
policy before pinning.

### T-202 — Implement `MediaSanitizer`
**Owner:** rust-developer
**Crate:** `zeph-sanitizer`
**Spec refs:** §3.4, §8 (Threat Model controls #3)
New file `crates/zeph-sanitizer/src/media.rs`: `MediaSanitizer` struct configured from
`McpMediaConfig`; `sanitize_image(&self, bytes: &[u8], declared_mime: &str, server_id: &str) -> Result<zeph_llm::ImageData, MediaRejected>`
implementing, in order: magic-byte sniff vs. declared MIME, format allowlist, byte-size cap,
`spawn_blocking` decode with `max_dimension_px`/`max_pixels` enforcement. `MediaRejected`
`thiserror` enum: `SizeExceeded`, `DimensionExceeded`, `FormatNotAllowed`, `MimeMismatch`,
`DecodeFailed`. Depends on: T-201.

### T-203 — `MediaSanitizer` unit tests
**Owner:** rust-agents:rust-testing-engineer
**Crate:** `zeph-sanitizer`
**Spec refs:** AC-4
`test_media_sanitizer_accepts_valid_png/jpeg/gif/webp`,
`test_media_sanitizer_rejects_size_exceeded`,
`test_media_sanitizer_rejects_dimension_exceeded` (crafted small-file/huge-declared-dimension
fixture),
`test_media_sanitizer_rejects_mime_mismatch`,
`test_media_sanitizer_rejects_disallowed_format`. Depends on: T-202.

### T-204 — `McpServerConfig.media_passthrough` + `McpMediaConfig`
**Owner:** rust-developer
**Crate:** `zeph-config`
**Spec refs:** §3.4
In `crates/zeph-config/src/channels.rs`: add `pub media_passthrough: bool` (`#[serde(default)]`)
to `McpServerConfig` (`:1344-`); add new `McpMediaConfig` struct (`max_image_bytes` default
5 MiB, `max_dimension_px` default 8192, `max_pixels` default 64_000_000, `max_images_per_result`
default 4, `max_images_per_turn` default 8, `allowed_formats` default
`["jpeg","png","gif","webp"]`); add `pub media: McpMediaConfig` (`#[serde(default)]`) to
`McpConfig` (`:1209-`).

### T-205 — Populate `ToolOutput.media` in the MCP executor
**Owner:** rust-developer
**Crate:** `zeph-mcp`
**Spec refs:** §3.2 (step 1), §8 (Threat Model control #1, #7)
In `crates/zeph-mcp/src/executor.rs`, `execute_tool_call` (`:96-137`): after the existing
`render_content_blocks` call, if the resolved server's `media_passthrough` is `true` and its
`trust_level != Sandboxed`, iterate `result.content` for `ContentBlock::Image` blocks (capped at
`max_images_per_result`), decode the block's base64 `data` and pass through
`MediaSanitizer::sanitize_image`, push successes into `ToolOutput.media`. Log every accept/reject
via the existing tool audit path (server id, tool name, mime, byte count, outcome). Depends on:
T-202, T-204.

### T-206 — Deferred-marker comment on `render_content_block`
**Owner:** rust-developer
**Crate:** `zeph-mcp`
**Spec refs:** §1 (Out of Scope), invariant #4
Add `// TODO(#5366): Audio/blob/resource-link MCP passthrough deferred — Audio needs Ask-First MessagePart::Audio variant (invariant #4); see specs/072`
above the `ContentBlock::Image` arm in `render_content_block` (`content.rs:58-60`).

### T-207 — MCP opt-in gating tests
**Owner:** rust-agents:rust-testing-engineer
**Crate:** `zeph-mcp`
**Spec refs:** AC-1, AC-2, AC-3
`test_executor_populates_media_only_when_opted_in`,
`test_executor_sandboxed_server_never_populates_media`. Depends on: T-205.

### T-208 — Sibling `MessagePart::Image` emission in `process_one_tool_result`
**Owner:** rust-developer
**Crate:** `zeph-core`
**Spec refs:** §3.2 (step 4), §3.3 (vision-tier gate), §5 (edge cases)
In `crates/zeph-core/src/agent/tool_execution/tool_result.rs`, `process_one_tool_result`
(`:369-477`), immediately after `result_parts.push(MessagePart::ToolResult{..})` (`:471-475`):
if `!is_error && !vigil_blocked && !classification.media.is_empty()`, resolve the vision-tier
gate (T-209) and, if it resolves affirmatively, push one `MessagePart::Image(Box::new(img))` per
entry (respecting a running per-turn counter capped at `max_images_per_turn`, threaded in from
`process_tool_result_batch`); otherwise `tracing::warn!` and drop. Depends on: T-002, T-006,
T-209.

### T-209 — Vision-tier "requires-vision" routing signal
**Owner:** rust-developer
**Crate:** `zeph-llm`
**Spec refs:** §3.3 (S3), §4 (C3)
In `crates/zeph-llm/src/router/triage.rs` (and any sibling router/cascade strategy module):
introduce the mechanism by which a caller with a pending message set containing a tool-result
`Image` part can determine, before the next `chat_with_tools` call, whether the concretely
selected tier for that call will be vision-capable. If it cannot be guaranteed, the caller
(T-208) must drop the `Image` parts. The exact mechanism (forcing tier selection vs. a
query-then-decide API) is an implementation choice; the only pinned, testable rule is: a turn
carrying an unresolved-vision `Image` part never reaches a provider as a 400/422. This may
require a small new method on the router provider trait/impl — keep it additive
(`#[non_exhaustive]`-compatible), do not break the existing `supports_vision()` contract used by
non-routed single providers.

### T-210 — Vision-tier gate tests + mandatory live session test
**Owner:** rust-agents:rust-testing-engineer + rust-agents:rust-live-tester
**Crate:** `zeph-llm`, `zeph-core`
**Spec refs:** AC-6
`test_vision_tier_gate_never_sends_image_to_incapable_tier` (automated regression). Separately,
per the LLM Serialization Gate: run a live cascade + MCP-image session
(`cargo run --features full -- --config .local/config/testing.toml`) with a mock or real
image-returning MCP server behind a mixed-capability cascade pool; confirm no 400/422 in the
debug dump. Document the live-test result in the PR description before merge. Depends on: T-208,
T-209.

### T-211 — Error/quarantine/cap edge-case tests
**Owner:** rust-agents:rust-testing-engineer
**Crate:** `zeph-core`
**Spec refs:** AC-7, AC-8, AC-13
`test_process_one_tool_result_drops_media_on_error`,
`test_process_one_tool_result_drops_media_on_quarantine`,
`test_process_one_tool_result_respects_per_result_and_per_turn_caps`. Depends on: T-208.

### T-212 — Static system-prompt caveat
**Owner:** rust-developer
**Crate:** `zeph-core` (system-prompt/context assembly module)
**Spec refs:** §5 (edge case: system-prompt caveat), FR-011, AC-12
Locate the session/config-time system-prompt assembly path (not per-turn); when any configured
MCP server has `media_passthrough = true`, append one static caveat line (see plan.md P2 item 8
for suggested wording). Test: `test_system_prompt_caveat_static_across_turns` — asserts the
assembled system prompt is byte-identical across two consecutive turns (cache-safety proof).

### T-213 — Pre-assembly pass safety regression test (M6/C5/AC-15)
**Owner:** rust-agents:rust-testing-engineer
**Crate:** `zeph-core`
**Spec refs:** §4 (C5), AC-15
Critic-hardening item found on re-review of the corrected emission point. Build a batch of ≥2
tool calls in `process_tool_result_batch` where one tool result carries a `MessagePart::Image`
sibling positioned between two other `ToolResult` parts in `result_parts`. Run the full pass
sequence (`run_causal_ipi_post_probe` → `record_shadow_event` → `apply_acon_compression`) and
assert: (a) `apply_acon_compression`'s output for the `ToolResult` not adjacent to the `Image`
part is identical to a control run without the `Image` sibling present (proves the compression
pass's `tool_use_id`-based targeting is unaffected by the interleaved non-`ToolResult` part), and
(b) the `Image` part surviving into the final assembled `Message.parts` is `==` (same
`mime_type`, same `data`) to the value pushed at `tool_result.rs:475` (proves none of the three
passes mutates or drops it). This is a regression guard, not new production code — if it ever
fails, the fix is in whichever of the three passes stopped treating non-`ToolResult` parts as
opaque, not in this test. Depends on: T-208 (the corrected emission point must exist first).

---

## Phase P3 — Config Surface, CLI, TUI, Migration, Docs

### T-301 — `--init` wizard prompt
**Owner:** rust-developer
**Crate:** `src/` (binary)
**Spec refs:** FR-010, AC-11
In `src/init/mcp.rs`, add a per-server prompt: "Enable image passthrough for this server?"
default No. Golden/integration test for the wizard flow.

### T-302 — `--migrate-config` step
**Owner:** rust-developer
**Crate:** `zeph-config`
**Spec refs:** FR-009, AC-10
In `crates/zeph-config/src/migrate/mod.rs`, add a new migration step (next available number):
add `media_passthrough = false` to every existing `[[mcp.servers]]` entry if absent; add
`[mcp.media]` with full defaults if absent. Tests: idempotency (run twice, second is a no-op),
correctness (existing servers gain the field without altering other fields).

### T-303 — TUI status indicator
**Owner:** rust-developer
**Crate:** `zeph-tui` (or wherever the existing tool-status spinner plumbing lives)
**Spec refs:** CLAUDE.md "TUI Rules", plan.md P3 item 3
Add a `"Decoding MCP image…"` spinner/status line during `MediaSanitizer::sanitize_image`'s
`spawn_blocking` decode, and a source-labeled indicator when an image is actually attached to
the outgoing provider request.

### T-304 — CLI kill-switch (optional)
**Owner:** rust-developer
**Crate:** `src/` (binary)
**Spec refs:** plan.md P3 item 4 (`should` priority)
Add `--no-mcp-media` global flag forcing `media_passthrough` off for the process. Non-blocking
for spec acceptance if deprioritized.

### T-305 — Testing playbook
**Owner:** rust-agents:rust-live-tester
**Path:** `.local/testing/playbooks/mcp-media-passthrough.md` (main repo root, not worktree — per `.claude/rules/continuous-improvement.md`)
**Spec refs:** all ACs
New playbook covering opt-in round-trip, Sandboxed override, malformed/oversized rejection,
cascade vision-tier routing (manual steps mirroring T-210's live test), persistence-exclusion
verification, `--migrate-config`/`--init` walkthroughs.

### T-306 — Coverage-status rows
**Owner:** rust-agents:rust-live-tester
**Path:** `.local/testing/coverage-status.md` (main repo root)
**Spec refs:** all
Add rows (status `Untested` initially, linking T-305's playbook) for: MCP media opt-in gating,
`MediaSanitizer` validation classes, ephemeral persistence strip, vision-tier routing gate,
`--migrate-config`/`--init` wiring. Update existing rows in place per project convention — do not
add new session/CI-cycle headers.

### T-307 — CHANGELOG entry
**Owner:** rust-developer
**Path:** `CHANGELOG.md`
Add an `[Unreleased]` entry describing the opt-in MCP image passthrough feature and the
`--migrate-config` config-shape addition.

### T-308 — Docs update (if applicable)
**Owner:** rust-agents:tech-writer
**Path:** `docs/src/`
**Spec refs:** §11
If an MCP configuration chapter exists in the mdBook docs, document `media_passthrough` and
`[mcp.media]`. Run `mdbook build` to verify.

---

## Follow-up (outside this PR chain, filed by team-lead)

### T-F01 — MATRA threat-model entry
**Owner:** team-lead (files issue, not a task in this chain)
**Spec refs:** §12 (See Also, spec-069)
File a follow-up issue to add an MCP-media asset/attack-tree entry to
`specs/069-threat-model/spec.md` — out of scope for this PR chain since spec-069 is a
separately-maintained living document.

---

## Task Dependency Summary

```
T-001 → T-002 → T-003
T-002 → T-004 (independent, no ordering requirement beyond T-002 existing)
T-002, T-005 → T-006
T-001..T-006 → T-007 (P0 regression gate)

T-101 → T-102, T-103

T-201 → T-202 → T-203
T-204 (independent of T-201/T-202)
T-202, T-204 → T-205 → T-206, T-207
T-002, T-006 → T-208
T-209 (independent design task, feeds T-208)
T-208, T-209 → T-210, T-211
T-208 → T-213 (M6/C5 pre-assembly pass safety regression)
T-212 independent of T-208 (separate assembly path)

T-301, T-302 depend on T-204 (config fields must exist)
T-303 depends on T-202 (decode step to instrument)
T-305, T-306 after P2 PR merged
T-307, T-308 after P3 deliverables land
T-F01 filed any time after spec approval, independent of implementation
```
