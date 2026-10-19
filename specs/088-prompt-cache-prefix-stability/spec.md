---
aliases:
  - Prompt Cache Prefix Stability
  - Context Layout
  - TurnContext
tags:
  - sdd
  - spec
  - context-management
  - prompt-caching
  - claude
created: 2026-10-19
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[004-memory/spec]]"
  - "[[005-skills/spec]]"
  - "[[021-zeph-context/spec]]"
  - "[[082-per-message-usage-cost-tracking/spec]]"
---

# Spec: Prompt Cache Prefix Stability (`[memory] context_layout`)

> [!info]
> Makes consecutive Claude requests byte extensions of each other so the provider prompt cache (system, tools, history) survives from turn to turn and preserved thinking stays valid. Promoted from the research draft "Compaction Cache-Prefix Stability" (CI cycle 1443). GitHub #6846 (prefix stability), #6847 (streamed usage), #6848 (typed path through Masked/Any), #6563 (slice: FR-004/005 plus batch eviction).

## 1. Problem

Before this spec every turn rewrote the head of the request: the system prompt was rebuilt (active skill bodies, volatile block), recalled context was removed and re-injected at the context floor, per-turn sweeps pruned old tool output, and thinking from earlier turns was dropped. Each of these changes bytes upstream of the cache breakpoint, so the cache was rebuilt in full on every turn.

## 2. Layouts

`[memory] context_layout = "auto" | "replace" | "append"` (default `auto`).

| Layout | Behavior |
|---|---|
| `replace` | Legacy behavior, byte-identical to the pre-change baseline: per-turn system rebuild, head injection at the context floor, per-turn sweeps, model-based thinking scope |
| `append` | Frozen session prompt, delta directives, per-turn `TurnContext` on the turn input, verbatim thinking, pressure-gated sweeps, watermark trim, prefix guard |
| `auto` | Resolved at pin time from `LlmProvider::prefix_cache()`: `Prefix` gives `append`, `None` gives `replace` |

- The layout is pinned per conversation (`MessageState.layout: LayoutState` holding `PinnedLayout { Replace, Append { key } }`, reset by `/clear` and `/new`, pinned on the first turn after a reset). Hot-reloading `context_layout`, a provider switch, or `auto` re-resolution takes effect only at the next `/clear`; a pending change logs once (`core.context.layout_pending`)
- `MemoryFirst` and `Adaptive` context strategies drain history every turn and always resolve to `replace`, so a conversation never mixes the artifacts of both layouts
- `ConversationKey` is a fresh random `u64` generated at each pin
- TUI `/status` shows the effective layout and a pending notice. There is no CLI flag

## 3. Functional Requirements

| ID | Requirement | Status |
|---|---|---|
| FR-001 | Choose a compaction shape that evicts only after the last cache breakpoint | deferred (follow-up) |
| FR-002 | Prefer the cache-preserving shape when it relieves enough pressure | deferred (follow-up) |
| FR-003 | Budget correctness wins over cache economics | holds: trim and compaction are unchanged in correctness |
| FR-004 | Prefix invalidation is an observable event | met: `llm.claude.prefix_guard{reason, wire_index, history_index}` span |
| FR-005 | Behavior is configurable; deployments without prompt caching see no change | met: `replace` is byte-identical, `auto` stays `replace` without `Prefix` |
| FR-006 | In `append`, request N+1 is a strict byte extension of request N (system, tools, messages, `cache_control` stripped) | met by construction plus guard |
| FR-007 | Thinking of earlier turns remains present in `append` | met: `ThinkingScope::Verbatim` via `RequestPolicy` |
| FR-008 | No turn-scoped `System` message beyond index 0 | met: directives are a wire-level rendering only |

## 4. TurnContext

`MessagePart::TurnContext(TurnContextBlock { kind: Directives | Retrieved, trust, text, keys })` (contract in spec 001 section 4).

- At most two blocks per turn input: all retrieved sources concatenated into one `Retrieved` block (sanitized and spotlighted per section as before), all directive sections in one `Directives` block. `trust` is the minimum over sections
- Cap: `turn_context_budget_ratio = 0.05` of `max_tokens`, validated `(0, 0.25]` (about 9k retrieved tokens per turn at a 200k budget, versus a 30% slice per turn in `replace`). The reduction is deliberate: item delta makes the per-turn cost recurring
- Item delta: every item carries `RetrievalKey(u64)`, a blake3 content hash of (source, item) (`ItemKeys` in `zeph-context`). Fetchers skip keys present in `TurnContext` parts at or after `context_floor`, so there is no separate ledger to desync after compaction or trim
- Rendering (Claude): `Retrieved` is a user text block before the user text, so memory carries no system authority. `Directives` is a wire `role:"system"` message only when the turn input is last or directly followed by an assistant message (models with `mid_conversation_system`), otherwise a user text block after the user text. Other providers render both as user text. A 400 "not supported" latches the user-text fallback once

## 5. Frozen session prompt and delta directives

- `LayoutState.rendered` holds the key of the rendered prompt: hashes of the skills catalog (plus the sorted `(skill, trust severity)` entries), the instruction blocks, MCP server instructions, environment without cwd and branch, `repo_map_tokens` and `group_structured`. The inputs of the last render are kept so a boundary can re-render in the same turn
- Re-render boundaries: session start, `/clear`, hard compaction, history trim and a trust revocation. Hard compaction and trim re-render in the same turn, right after `prepare_context` (`rerender_invalidated_prompt`), so the system change lands in the request that is rewritten anyway; `complete_focus`/`compress_context` change no frozen-prompt input, so they only rewrite history and do not invalidate the prompt. Server compaction does not re-render (it keeps the tools and system cache)
- Trust revocation is a boundary that also drops every `Directives` part from history: a skill that left the catalog (blocked or removed) or got a higher trust severity (`/trust`, quarantine, hash mismatch on reload), a skill body sent at a lower severity than it has now, or instruction text that disappeared. Skill body keys include the trust severity, so a demoted skill is sent again in its sanitized form
- Known cost: a revocation fires on any demotion of a catalog skill, including a learning auto-demote of a skill that is not active this turn, or a transient trust-map miss; each costs one full cache miss. The triggering skill is logged by `core.context.directive_purge`. Auto-demote is a real trust change, so it is treated as one
- After a same-turn boundary (compaction, trim, eviction) the turn's directives are rebuilt against the post-boundary history (`rebuild_directives_if_history_changed`) and retrieval is fetched again after a trim, so content carried by removed messages is not missing for the boundary turn
- Between boundaries, additions and upgrades (catalog, server instructions, cwd) become delta sections of the next `Directives` block
- Two kinds of directive section. Content-addressed items (skill bodies, MCP tool prompts, ERL suffix, caveman and `memory_save` notices) are sent once: a key already present in history is skipped. State sections (environment, catalog/instruction/MCP-instruction updates, preferences, goal) are compared with the **last value sent for that source**, so A to B to A is announced again, and a cleared value sends `<source_cleared/>` once. Each state section stamps `[slot, value]` keys on its block; the last value is read from history, so compaction drops it together with the message
- Directives contain: the names-only `<active_skills>` line every turn, skill bodies (none in compact or fallback mode), matched MCP tools keyed per tool, environment (cwd and branch), preferences, goal, notices, budget, time
- `skill_reload.rs` no longer writes `messages[0]` in `append`

## 6. Request policy and prefix guard

- `RequestPolicy { thinking, conversation }` is stamped into `messages[0].metadata` (`#[serde(skip)]`) before every provider call. `append` stamps `Some { Verbatim, key }`, `replace` stamps `None`
- Guard (Claude, only with `Some`): fingerprints of the wire projection, keyed by `ConversationKey`, bounded to 16 entries. On divergence at wire index `k` thinking is dropped from `k` as a trailing run and reported via `ThinkingReplay::Dropped`. The fingerprint is committed once the provider accepts the response (HTTP 200 for streams, before the stream is consumed). Full contract in spec 003

## 7. Sweeps, trim and scrub (#6563 slice)

- `append`: a per-turn sweep gate (`decide_sweeps`, span `core.context.sweep_gate`) guards `prune_stale_tool_outputs`, `truncate_old_tool_results`, time microcompact, SideQuest eviction and deferred tool-pair summaries. The gate is closed below the soft compaction tier, opens once on entering the soft band, re-arms only after the prompt grew by 10% over its size after the last sweep, and is always open at the hard tier. The mid-iteration summary apply and soft prune in the tool loop run behind the same gate, at most once per turn (or at the hard tier). The deferred-summary count trigger is never used in `append`
- Trim uses `history_trim_watermark = 0.75` (validated `(0, 1]`, hot-reloadable): a trim brings history to `slot * watermark` and following turns append without trimming. A turn whose cached prompt size is already under the slot skips the history scan
- The unsent tail is credential-scrubbed in place just before the send (`core.context.scrub_tail`), so a sent message is never edited. Text parts of user and assistant messages get the full scrub (secrets, URL credentials, paths; the stricter choice, even though it also redacts paths in the assistant narration); tool results and tool outputs lose secrets only (paths are data). `content` is rebuilt from the scrubbed parts. Progress is an index plus a fingerprint of the last scrubbed message; if it no longer matches (trim, compaction, pop) the whole window is scrubbed again (idempotent). Enabling redaction by reload does one full pass (one guard event)
- Retrieval keys are recorded per source and discarded when the section ends up empty after sanitization, so those items stay eligible. Tiered recall is bounded by the turn cap and deduplicated as one item
- Subagents: `trim_parent_messages` strips `TurnContext`; subagent loops keep their own behavior (no hint is passed to them)

## 7a. Known limits of the `append` layout

- CAM fidelity scoring is skipped (it rewrites sent history); GoSkills grouping falls back to flat per-skill bodies
- Content-addressed directives (caveman, `memory_save` hint) are sent when first needed; turning one off later does not retract the earlier text. State sections do track changes and clears
- Subagent loops build their system prompt once and never rebuild it per iteration, so they need no boundary handling

## 8. Integration points

| Point | Change |
|---|---|
| config.toml | `context_layout`, `turn_context_budget_ratio`, `history_trim_watermark` |
| `--migrate-config` | `migrate/memory.rs` step |
| `--init` | layout select in `src/init/memory.rs` |
| TUI | `/status` effective layout |
| Spans | `core.context.layout_pin`, `core.context.layout_pending`, `core.context.session_prompt.render{reason}`, `agent_context.service.collect_turn_context{turn_context_tokens}`, `agent_context.service.trim_history{removed,target}`, `llm.claude.prefix_guard`, `llm.claude.stream_usage`, `llm.any.chat_typed`, `llm.masked.chat_typed` |

## 9. Deferred

- FR-001/002 cache-breakpoint-aware compaction shape, TTL-aware soft tier deferral, OpenAI automatic prefix-cache semantics for `prefix_cache()`, SC-001/002 baselines
- D1: StickyToolOffer growth sent as `tool_addition` blocks (beta `mid-conversation-tool-changes-2026-07-01`)
- D2: `clear_at` turn-scoped Time/Budget directives (beta `mid-conversation-system-clear-at-2026-08-21`)
- Default flip: ships `auto` only if the live gate passes (4+ turns with tools, code index and graph: no `thinking_dropped`, zero guard spans in steady state, `cache_read_input_tokens` growing); otherwise `replace` is the default

## 10. Key Invariants

- `replace` requests stay byte-identical to the pre-change baseline, including a Router fixture serving one `append` and one `replace` conversation in alternation over shared sub-providers
- The layout never changes inside a conversation
- No instance-level mutable policy state: policy travels per request, fingerprints are keyed per conversation
- `TurnContext` is never persisted and never summarizer input

## 11. Agent Boundaries

### Always
- Name `TurnContext` explicitly in every `MessagePart` match; use `to_request_text` in content branches

### Ask First
- Changing the re-render boundaries of the frozen prompt; adding a second `TurnContext` kind or a third block per turn

### Never
- Push a turn-scoped `System` message beyond index 0
- Write `messages[0]` from skill reload or MCP late connect in `append`
- Edit a message after it has been sent in `append`
