---
aliases:
  - Skill Execution State
  - SKILL.state
  - Bounded Execution State
  - Skill State Patch
tags:
  - sdd
  - spec
  - skills
  - core
  - context
  - contract
created: 2026-09-05
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[005-skills/spec]]"
  - "[[021-zeph-context/spec]]"
  - "[[034-zeph-bench/spec]]"
  - "[[064-durable-execution/spec]]"
---

# Spec: Skill Execution State (SKILL.state)

> [!info]
> Opt-in, schema-declared bounded execution state for skill-driven tool loops. A skill may
> declare a small structured `state:` block in its SKILL.md frontmatter; once active, the
> model patches that state through a built-in tool instead of re-reading the growing tool-call
> history, and the tool results already absorbed into the state are cleared from context via
> the existing `microcompact` sentinel discipline. Additive at every layer — a skill without a
> `state:` block sees zero behavior change. Design produced by a four-pass architect/critic
> review (`.local/handoff/2026-09-05T23-27-59-architect.md`,
> `.local/handoff/2026-09-05T23-40-18-critic.md`,
> `.local/handoff/2026-09-05T23-46-11-architect.md`,
> `.local/handoff/2026-09-05T23-50-46-critic.md`), the last pass returning verdict `minor`.

## Sources

### External
- [SKILL.state: Bounded Execution State for Long-Horizon Agents (arXiv:2608.26263)](https://arxiv.org/abs/2608.26263)
  — motivating paper. Taken on faith: the O(T) vs. O(T²) accumulation shape, and that a fixed
  schema is expressible for procedural skills. **Not validated for Zeph and MUST NOT be cited
  as fact in this codebase**: the paper's 16.2× token reduction, 54.2% InterCode pass rate,
  noise-robustness, and zero-hallucinated-recovery figures were measured on InterCode CTF /
  τ-Bench, neither of which resembles Zeph's workloads. A material driver of the paper's ratio
  is discarding chain-of-thought after each step — Zeph forwards thinking blocks verbatim
  (`specs/002-agent-loop/spec.md:93`) and this spec clears only `ToolOutput`/`ToolResult`
  content, so Zeph's ceiling is materially lower. See § Known Limitations.
- GitHub #6749 (this spec's driving issue) / #6750 (companion issue; orchestration half
  refuted, subagent half deferred — see § Overview).

### Internal
| File | Contents |
|---|---|
| `crates/zeph-skills/src/loader.rs:129-133` | `SkillMeta` struct — gains `state: Option<SkillStateSchema>` |
| `crates/zeph-skills/src/loader.rs:489-587` | `parse_frontmatter` — nested-block handling generalized (D5) |
| `crates/zeph-skills/src/extensions.rs:159-178` | `SkillExtensions` / `parse_extensions` — sibling pattern for the new `state:` block |
| `crates/zeph-skills/src/trust.rs:105` | `compute_skill_hash` — hashes the whole SKILL.md file; adding `state:` re-triggers `requires_trust_check` re-attestation |
| `crates/zeph-skills/src/generator.rs:412,448` | Self-learning re-emission of whole SKILL.md text; `metadata` feeds re-emission |
| `crates/zeph-skills/src/merge_prompts.rs:18` | `MERGE_SYSTEM_PROMPT` — must preserve `state:` block on skill merge |
| `crates/zeph-core/src/agent/state/mod.rs:83-168` | `SkillState` — new state field lives here |
| `crates/zeph-core/src/agent/state/mod.rs:907-953` | `ToolState` — gains `last_batch_tool_call_ids`; sibling of `current_tool_iteration` (which has the never-reset-per-turn defect, M2) |
| `crates/zeph-core/src/agent/state/mod.rs:1038` | `durable_agent_turns_config: Option<DurableConfig>` — session-stable gate predicate for D1 |
| `crates/zeph-core/src/agent/context/assembly.rs:908` | `active_skill_names` assignment site — where skill-state activation is resolved, turn-start only |
| `crates/zeph-core/src/agent/context/assembly.rs:1058` | Cache-stable system-prompt prefix seal — state must never enter here |
| `crates/zeph-core/src/agent/context/assembly.rs:1106-1107` | Volatile system-prompt region ("never cached") |
| `crates/zeph-core/src/agent/context/assembly.rs:1177` | `BudgetHint` reads `current_tool_iteration` once per turn (evidence for D4, M2) |
| `crates/zeph-core/src/agent/tool_execution/tier_loop.rs:2331` | `call_llm_durable` fingerprint construction (`fp_input`) — D1's hazard |
| `crates/zeph-core/src/agent/tool_execution/tier_loop.rs:2466-2474` | `handle_native_tool_calls` → clearing pass insertion point → `maybe_summarize_tool_pair` → `prune_stale_tool_outputs` |
| `crates/zeph-core/src/agent/llm_dispatch.rs:92-113` | Rolling-tail rendering precedent (`remove_lsp_messages` → `push_message` → `recompute_prompt_tokens`) — D4's seam |
| `crates/zeph-core/src/agent/utils.rs:287-294` | `recompute_prompt_tokens` — re-tokenizes every message; O(history) cost accepted by D4 |
| `crates/zeph-context/src/microcompact.rs:14-45` | `LOW_VALUE_TOOLS`, `CLEARED_SENTINEL_PREFIX`, `find_preceding_tool_use_name` — reused primitives |
| `crates/zeph-context/src/microcompact.rs:72-140` | `sweep_stale_tool_outputs` — not directly reusable (D3); pattern reference only |
| `crates/zeph-llm/src/provider.rs:279-305` | `MessagePart::ToolResult` (has `tool_use_id`) vs. `ToolOutput` (has `compacted_at`, no id) |
| `crates/zeph-tools/src/tool_result.rs:517` | Native-loop batches built as `ToolResult` — primary id-matching path |
| `crates/zeph-durable/src/handle.rs:661-670` | Replay fingerprint-mismatch abort (`ExecutionStatus::Aborted`) |
| `crates/zeph-core/src/agent/builder.rs:2570` | `AgentBuilder::with_durable_agent_turns` — sole writer of the D1 gate field |
| `crates/zeph-core/src/agent/durable_bootstrap.rs:206-214` | `ensure_session_durable_ctx` — lazy `durable_ctx` opener, distinct from the gate field |
| `.zeph/skills/skill-audit/SKILL.md` | Concrete pilot skill (D6) — enumerate/audit/report procedure |

---

## 1. Overview

### Problem Statement

A class of skills reduces many step-by-step tool observations into a small final artifact and
never needs the raw observations again once each is absorbed. `.zeph/skills/skill-audit/SKILL.md`
is the concrete case: it enumerates every skill directory, `cat`s each full SKILL.md body, and
reduces it to a fixed verdict tuple (spec pass/warn/fail, security safe/warn/fail, rating,
reason) before building a report **purely from the accumulated verdicts** — yet today every full
SKILL.md body it read stays in conversation history for the rest of the run. This is the O(T²)
shape arXiv:2608.26263 targets: cumulative context grows with every step even though the model
only ever needs the latest reduction, not the raw trace.

### Goal

Let a skill declare a small, fixed schema for its own bounded execution state. When such a skill
is active, the model maintains that state through a validated patch tool instead of re-deriving
it from history, and the tool results already folded into a patch are cleared from context using
the existing `microcompact` sentinel-overwrite discipline (replace content, keep the message and
its `ToolUse`/`ToolResult` pairing). A skill that declares no `state:` block sees no behavior
change at any layer.

### In Scope

- `zeph-skills`: `state:` frontmatter block parsing, `SkillStateSchema` type, patch validation
  and dictionary-merge semantics, the `parse_frontmatter` nested-block generalization (D5) that
  incidentally fixes a pre-existing `extensions:` leak into `SkillMeta.metadata`.
- `zeph-core/src/agent/`: `SkillExecutionState` runtime object, `skill_state_patch` built-in
  tool, the post-validation clearing pass in `process_single_native_turn`, and the rolling-tail
  `<skill_state>` rendering hook.
- `zeph-context::microcompact`: new pure helper `clear_absorbed_tool_results` and a sibling
  `find_preceding_tool_use_id`.
- `zeph-config`: `[skills.state]` section, `--migrate-config` step.
- Required integration points per CLAUDE.md Development Rules (config, CLI, TUI, `--init`,
  migration, playbook, coverage row) — see § 8.
- `.zeph/skills/skill-audit/SKILL.md` ships with a `state:` block in the same PR as the
  mechanism (D6), as the subject of the gating `zeph-bench` A/B.

### Out of Scope

- A general-purpose compaction replacement. This mechanism only clears `ToolOutput`/`ToolResult`
  content that a validated patch has explicitly absorbed — it does not replace, extend, or
  couple to `CompactionState`, summarization, or eviction scoring.
- Persistence of `SkillExecutionState` across a history reload. In-memory only for this spec
  (see § Known Limitations); durable/session persistence is `zeph-session`'s domain (spec 068),
  not adopted here.
- The `zeph-orchestration` half of GitHub #6750. **Premise refuted**: DAG nodes already receive
  a bounded, sanitized, char-capped `<completed-dependencies>` block
  (`crates/zeph-orchestration/src/scheduler/router.rs:25-116`), and `TaskResult.output` is a flat
  `String` (`graph.rs:289-291`) — no `Vec<Message>` transcript exists to bound. There is nothing
  in `zeph-orchestration` for this mechanism to attach to.
- Presenting a group-structured or multi-schema view when more than one state-declaring skill
  is active in the same turn — see § 7 Edge Cases for the single-primary-schema rule adopted
  instead.

### Deferred (design constraint only, not implemented here)

The `zeph-subagent` half of #6750 is **confirmed** (single unbounded `Vec<Message>` history,
`crates/zeph-subagent/src/agent_loop.rs:578-581`, no `zeph-context` dependency, only a FIFO
message-count trim) but is a separate lift — the crate has no context machinery to extend. This
spec requires that `SkillStateSchema`, `SkillExecutionState`, and `StatePatch` live in
`zeph-skills` with **zero dependency on `zeph-core`**, so a future `zeph-subagent` adoption needs
no redesign of the schema/patch/merge layer, only new wiring in the subagent loop.

---

## 2. User Stories

### US-001: A skill declares a bounded state schema

AS A skill author
I WANT to declare a small structured schema in my SKILL.md frontmatter
SO THAT the agent tracks my skill's running state instead of re-deriving it from the growing
tool-call history

```
GIVEN a SKILL.md with a `state:` frontmatter block
WHEN the skill loads
THEN SkillMeta.state is populated with a SkillStateSchema
AND a SKILL.md with no `state:` block yields SkillMeta.state == None with no parse cost delta
```

### US-002: The model patches state through a validated tool

AS A model executing a state-declaring skill
I WANT to submit incremental updates to my skill's state
SO THAT I do not need to re-read prior tool output already reduced into that state

```
GIVEN a state-declaring skill is the turn's active skill
WHEN the tool loop begins
THEN a `skill_state_patch` tool is registered with an input schema derived from the skill's
  declared fields
AND the tool is NOT registered when no state-declaring skill is active
```

### US-003: An invalid patch retries deterministically

AS A model producing a malformed patch
I WANT a clear, deterministic error instead of silent acceptance or a wasted extra LLM call
SO THAT I can correct the patch on my next turn of the tool loop

```
GIVEN a `skill_state_patch` call whose values fail schema validation
WHEN the pass validates the patch before merge
THEN the patch is rejected, prior valid state is unchanged, and the validation error is
  returned as the tool's next observation
AND no rollback of previously-merged state occurs and no extra LLM call is made
```

### US-004: Absorbed tool results are cleared from context

AS the agent loop
I WANT the tool results a validated patch has absorbed removed from the growing history
SO THAT the skill's context cost is bounded by the state object's size, not by turn count

```
GIVEN a `skill_state_patch` call in iteration N validates and merges
WHEN the post-validation clearing pass runs
THEN exactly the ToolResult/ToolOutput parts from the immediately preceding tool batch (by
  tool_use_id) are sentinel-cleared in place
AND no ToolUse part is ever removed and no ToolResult/ToolOutput is ever orphaned
AND clearing is idempotent — a part already carrying the sentinel or a set `compacted_at` is
  skipped
```

### US-005: A durable session is unaffected

AS an operator running with durable agent turns enabled
I WANT skill-state mode to be completely inert
SO THAT the exactly-once replay fingerprint never diverges because of in-memory-only state
mutation

```
GIVEN `durable_agent_turns_config.is_some()` for the session
WHEN a state-declaring skill would otherwise activate
THEN no `<skill_state>` block is rendered, no clearing pass runs, and a `tracing::warn!` fires
  once per session
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a SKILL.md has no `state:` frontmatter block THE SYSTEM SHALL set `SkillMeta.state = None` and introduce no behavior change at any layer | must |
| FR-002 | WHEN a SKILL.md declares a `state:` block THE SYSTEM SHALL parse it into a `SkillStateSchema` using the same textual sub-block extraction + `serde_norway` + byte-cap pattern as `SkillExtensions`, warn-not-fail on parse error | must |
| FR-003 | WHEN `parse_frontmatter` encounters any empty-valued top-level key THE SYSTEM SHALL enter a `nested` mode (`Collect` for `metadata`, `Skip` for every other key including `state` and `extensions`) so indented children never leak into `SkillMeta.metadata` | must |
| FR-004 | WHEN a state-declaring skill is the turn's active skill THE SYSTEM SHALL register the `skill_state_patch` built-in tool with an input schema derived from the declared fields, and SHALL NOT register it otherwise | must |
| FR-005 | WHEN `skill_state_patch` is called THE SYSTEM SHALL validate every supplied field against the declared schema (type, enum membership, numeric bounds, string length) before any merge | must |
| FR-006 | WHEN a patch fails validation THE SYSTEM SHALL reject the whole patch, leave prior state unchanged, and return the validation error as the tool call's observation — no partial merge, no rollback semantics needed, no extra LLM call | must |
| FR-007 | WHEN a patch passes validation THE SYSTEM SHALL merge it into `SkillExecutionState` as a shallow per-top-level-key dictionary merge, where a `null` value resets that key to its schema-declared default (or an empty collection if no default is declared) | must |
| FR-008 | WHEN a `skill_state_patch` call validates and merges in tool-loop iteration N THE SYSTEM SHALL record the `tool_use_id`s of the immediately preceding tool batch (iteration N's own inputs, already resident in `self.msg.messages`) in `ToolState.last_batch_tool_call_ids` | must |
| FR-009 | WHEN `ToolState.last_batch_tool_call_ids` would be read at tool-loop iteration 0 of a new turn THE SYSTEM SHALL have cleared it at turn start (or gated the clearing pass on `iteration > 0`), so a first-iteration patch in a new turn never clears a batch it never absorbed | must |
| FR-010 | WHEN a patch validates and merges THE SYSTEM SHALL run a post-validation pass, inserted between `handle_native_tool_calls` and `maybe_summarize_tool_pair` in `process_single_native_turn`, that calls `zeph_context::microcompact::clear_absorbed_tool_results` targeting exactly the ids in `last_batch_tool_call_ids` | must |
| FR-011 | `clear_absorbed_tool_results(messages: &mut [Message], tool_use_ids: &[String], sentinel: &str, now_ts: i64) -> usize` SHALL replace `ToolResult` content and set `ToolOutput.compacted_at` for matching parts, SHALL skip parts already sentinel-prefixed or already `compacted_at.is_some()`, and SHALL NEVER remove a `ToolUse` part or orphan a `ToolResult`/`ToolOutput` pairing | must |
| FR-012 | WHEN a `ToolOutput` part (no `tool_use_id`) needs id-matching THE SYSTEM SHALL provide `find_preceding_tool_use_id`, a sibling of `find_preceding_tool_use_name`, that walks back to the nearest preceding `ToolUse` in the same message | must |
| FR-013 | WHEN a state-declaring skill is active THE SYSTEM SHALL render a `<skill_state>` XML block at the rolling-tail seam in `llm_dispatch.rs` (remove stale `<skill_state>` system message → push freshly rendered one → `recompute_prompt_tokens()`), not at the once-per-turn system-prompt seam | must |
| FR-014 | WHEN the freshly rendered `<skill_state>` block is byte-identical to the currently installed one THE SYSTEM SHALL skip the remove/push/recompute step entirely | must |
| FR-015 | WHEN `durable_agent_turns_config.is_some()` for the session THE SYSTEM SHALL treat skill-state mode as inert for the whole session — no tool registration, no rendering, no clearing — and SHALL emit one `tracing::warn!` per session the first time a state-declaring skill would otherwise activate | must |
| FR-016 | WHEN more than one active skill in the same turn declares a `state:` block THE SYSTEM SHALL select exactly one as the turn's primary schema (highest matcher score, or the GoSkills entry-point skill when `group_structured = true`) and SHALL log a warning naming the excluded skill(s) — at most one `SkillExecutionState` object exists per turn | must |
| FR-017 | `SkillStateSchema`, `SkillExecutionState`, and `StatePatch` SHALL be defined in `zeph-skills` with zero dependency on `zeph-core`, so a future `zeph-subagent` adoption requires no redesign of these types | must |
| FR-018 | New config keys under `[skills.state]` SHALL be `#[serde(default)]`, and a `--migrate-config` step SHALL add the section with defaults to existing configs | must |
| FR-019 | WHEN a `state:` block is added to an existing skill THE SYSTEM SHALL trip `requires_trust_check` re-attestation for that skill via the existing whole-file `compute_skill_hash`, with no special-casing to exempt the new block from the hash | should |
| FR-020 | WHEN self-learning merges a skill body THE SYSTEM SHALL preserve an existing `state:` block through `MERGE_SYSTEM_PROMPT` rather than silently dropping it | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Patch validation is synchronous, deterministic schema checking — no LLM call, no I/O; must not violate the non-blocking hot-path contract (CLAUDE.md, spec 002's ban on synchronous subgoal extraction is the direct analog) |
| NFR-002 | Performance | The rolling-tail render (FR-013) costs `O(history)` per tool-loop iteration via `recompute_prompt_tokens` when the rendered block changes; FR-014's identity-skip is required specifically because, unlike the LSP-notes precedent this seam already pays for conditionally, `<skill_state>` would otherwise fire on every iteration unconditionally |
| NFR-003 | Isolation | `zeph-skills`'s new types add no dependency on `zeph-core`; `zeph-context`'s new helper adds no new dependency (the module is already imported by `zeph-core`) |
| NFR-004 | Correctness | Clearing must be schema-driven and deterministic, never score-based — this is not a `CompactionState` eviction and must never be confused with one (spec 021 §8 Ask-First item on `CompactionState` transitions is not triggered) |
| NFR-005 | Compatibility | Absent a `state:` block, parsing, tool registration, rendering, and clearing are all unconditionally no-ops — zero measurable behavior delta |
| NFR-006 | Auditability | Every new meaningful-work path carries a `tracing::info_span!` under `skills.state.*` (parse, validate, merge) or `core.skillstate.*` (render, clear, tool dispatch) |
| NFR-007 | Provider parity | Schema-constrained tool input is delivered to Ollama (`ollama.rs:629-648,678-699` transmits full `ToolFunctionInfo`) and candle-backed models identically to cloud providers; enforcement strength may vary by model, but the deterministic-retry mitigation (FR-006) is provider-independent |

---

## 5. Data Model / Key Types

| Entity | Location | Description |
|--------|----------|-------------|
| `SkillStateSchema` | `zeph-skills` | Declarative field-set parsed from the `state:` frontmatter block — sibling of `SkillExtensions`, same warn-not-fail parse contract, 8 KiB cap |
| `SkillExecutionState` | `zeph-skills` | The live, bounded state object for the active state-declaring skill — **in-memory only**, no persistence in this spec |
| `StatePatch` | `zeph-skills` | A validated, dictionary-merge patch with null-deletion semantics — rejected before merge on any validation failure, never partially applied |
| `NestedMode` | `zeph-skills::loader` | `enum { Collect, Skip }` — replaces `parse_frontmatter`'s `bool in_metadata`; `Collect` for `metadata:` (unchanged behavior), `Skip` for any other empty-valued top-level key (fixes the pre-existing `extensions:` leak, and covers the new `state:` block) |
| `ToolState.last_batch_tool_call_ids` | `crates/zeph-core/src/agent/state/mod.rs`, sibling of `current_tool_iteration` (`:924`) | `Vec<String>` — the `tool_use_id`s of the tool batch a validated patch in the current iteration may absorb; **must** be cleared at turn start or gated on `iteration > 0` (FR-009) |
| `clear_absorbed_tool_results` | `zeph-context::microcompact` (new, pure) | `fn(messages: &mut [Message], tool_use_ids: &[String], sentinel: &str, now_ts: i64) -> usize` — signature mirrors `sweep_stale_tool_outputs` (slice in, sentinel/`now_ts` supplied by caller, count returned); no `LOW_VALUE_TOOLS` gate, no `keep_recent` cutoff — targets exactly the given ids |
| `find_preceding_tool_use_id` | `zeph-context::microcompact` (new) | Sibling of `find_preceding_tool_use_name` — walks back to the nearest preceding `ToolUse` id for a `ToolOutput` part (which carries no `tool_use_id` itself) |
| `skill_state_patch` | Built-in tool, `zeph-core` | Registered only when a state-declaring skill is active; JSON input schema derived from `SkillStateSchema` |

### Schema Field Types (MVP grammar)

The `state:` block supports a fixed, small grammar sufficient for the pilot skill (§ 9) and
future candidates:

```yaml
state:
  <field_name>:
    type: string | integer | enum | list | object
    # string:  max_len (optional)
    # integer: min, max (optional)
    # enum:    values: [ ... ]  (required)
    # list:    items: <nested field spec>, max_items (optional)
    # object:  fields: { <name>: <nested field spec>, ... }
    default: <value>            # optional; used on null-deletion and initial state
```

### Merge Semantics

A patch is a JSON object keyed by top-level `state:` field names. For each supplied key:

- The value is validated against that field's declared type/constraints (recursively, for
  `list`/`object` fields).
- `null` resets the field to its declared `default`, or to an empty collection (`[]`/`{}`) if no
  default is declared.
- The merge is **shallow per top-level key** — a `list`/`object` field's new value fully replaces
  the old one (no partial array splice, no deep object merge). This keeps MVP validation simple;
  a finer-grained patch grammar is not required by the pilot skill and is not in scope.
- Any single key failing validation rejects the entire patch (FR-006) — there is no
  key-by-key partial acceptance.

---

## 6. Architecture

### 6.1 Schema Declaration and Parse-Time Fix (D5)

`SkillMeta` gains `state: Option<SkillStateSchema>` next to `extensions`
(`crates/zeph-skills/src/loader.rs:129-133`). The block is extracted by a new textual sub-block
extractor mirroring `parse_extensions` (`extensions.rs:159-178`): 8 KiB cap enforced before
`serde_norway::from_str`, parse failure returns `None` (warn-not-fail), absent block ⇒ `None`.

This alone is insufficient without fixing `parse_frontmatter`'s existing nested-block bug
(`loader.rs:489-587`): `in_metadata: bool` is set only for `key == "metadata" && value.is_empty()`
(`:580`); any *other* empty-valued top-level key (including the pre-existing `extensions:` and
the new `state:`) falls to the `_ =>` catch-all (`:489-494`), which drops the empty parent key —
but its indented children are then re-parsed as top-level pairs on the next lines and leak into
`SkillMeta.metadata`, which feeds self-learning re-emission (`generator.rs:448`). This is latent
today only because no shipped SKILL.md declares `extensions:`; adopting a second optional block
would make it live.

**Fix**: replace `in_metadata: bool` with `nested: Option<NestedMode>` where
`NestedMode ∈ { Collect, Skip }`:
1. On an empty-valued top-level key, set `nested = Some(if key == "metadata" { Collect } else { Skip })`.
2. While `nested.is_some()`, an indented line is consumed: `Collect` parses it into
   `raw.metadata` exactly as today; `Skip` discards it (owned by a separate textual pass —
   `parse_extensions` or the new `state:` extractor).
3. The first non-indented line clears `nested` and falls through unchanged.

Verified empirically across all 27 shipped SKILL.md files: `metadata:` is the only empty-valued
top-level key, so no shipped skill's parse output changes. One cosmetic divergence exists (not a
regression in practice): an empty-valued `requires-secrets:` would no longer emit its deprecation
warning — no shipped skill triggers this. Fold this fix into this feature's implementation (not a
separate PR) since the corrected algorithm fixes both `extensions:` and `state:` in one change.

Two secondary parse-time consequences to implement alongside FR-002/FR-003:
- `compute_skill_hash` (`trust.rs:105`) hashes the whole file, so adding `state:` to an existing
  skill trips `requires_trust_check` re-attestation (FR-019) — expected, not a defect.
- Self-learning re-emits whole LLM-generated SKILL.md text (`generator.rs:412`,
  `merge_prompts.rs:18`); `MERGE_SYSTEM_PROMPT` must be updated to preserve the `state:` block on
  merge, or it is silently dropped (FR-020).

### 6.2 Runtime State Object and Activation

`SkillExecutionState` is written at the same site `active_skill_names` is assigned
(`crates/zeph-core/src/agent/context/assembly.rs:908`) — turn-start only, never mid-turn, per the
"resolved at turn start" constraint already governing skill hot-reload
(`specs/005-skills/spec.md` § Construction-Time / Reload-Time Skill Prompt Contract). **In-memory
only for MVP** — spec 064 INV-1 forbids journaling domain types into Layer-0 infrastructure, and
its cross-spec note names `zeph-session` (spec 068) as the sanctioned persistence home; this spec
defers persistence entirely rather than pick the wrong layer.

When more than one active skill declares a `state:` block in the same turn (FR-016), the primary
is chosen deterministically (highest matcher score, or the GoSkills entry-point when
`group_structured = true` — see `specs/005-skills/spec.md` § GoSkills) and the rest are logged
and ignored for state purposes; at most one `SkillExecutionState` exists per turn.

### 6.3 Patch Tool and Validation

A built-in tool, `skill_state_patch`, is registered **only** when a state-declaring skill is
active (FR-004). Its JSON input schema is derived from the active skill's `SkillStateSchema` —
this is Zeph's existing equivalent of the paper's grammar-constrained decoding, directly relevant
to the malformed-output failure mode the paper reports for small models (NFR-007).

Validation runs before merge (FR-005). An invalid patch is rejected with a deterministic
observation (the validation error) fed back as the tool's next result — no rollback needed
because nothing was merged, and no extra LLM call, since this is a synchronous, cheap check
(NFR-001), never a repair-LLM round trip. (Per CLAUDE.md's multi-model design principle: if a
future repair path ever adds an LLM call, it must expose a `*_provider` config field; MVP has
none.)

### 6.4 Clearing the Absorbed History

`apply_acon_compression` (`tier_loop.rs:1978-1980`) is **not** the clearing seam — it mutates
`result_parts` before `Message::from_parts`, i.e. the *current* batch, while a patch that absorbs
iteration N's results is only emitted (and merges) in iteration N+1, by which point those
messages are already in `self.msg.messages`.

The clearing seam is a new post-validation pass inserted in `process_single_native_turn`,
between `handle_native_tool_calls` (`tier_loop.rs:2466-2470`) and `maybe_summarize_tool_pair`
(`:2472`) — a sibling of the existing `prune_stale_tool_outputs(keep_recent)` call (`:2474`),
which is the structural precedent for "mutate history in place at this point in the loop".
Running before `maybe_summarize_tool_pair` means absorbed output is not redundantly summarized
at cost in the same pass — though this ordering only *prevents*, never *causes*, redundant work:
`maybe_summarize_tool_pair` targets the oldest unsummarized pair beyond a cutoff, so with a small
cutoff the absorbed batch may already have been summarized in an earlier iteration. Do not treat
"never summarized" as a guarantee.

Targeting is by `tool_use_id`, not tool name — the ids in `ToolState.last_batch_tool_call_ids`
(FR-008/FR-009). `MessagePart::ToolResult` carries `tool_use_id` directly (native-loop batches
are built as `ToolResult`, `tool_result.rs:517` — the primary path); `ToolOutput` has no id and is
matched via `find_preceding_tool_use_id` (FR-012).

`clear_absorbed_tool_results` (FR-011) reuses only `CLEARED_SENTINEL_PREFIX`
(`microcompact.rs:33`) and the replace-content-keep-the-part discipline
(`microcompact.rs:125-138`) — `sweep_stale_tool_outputs` itself is not reusable: it is hard-gated
on `is_low_value_tool` against a fixed `LOW_VALUE_TOOLS` const and clears by a global
`keep_recent` cutoff, neither of which fits targeting specific absorbed results. Idempotency
guards mirror `sweep_stale_tool_outputs`'s own check (`:88-92`): skip parts already
sentinel-prefixed or with `compacted_at.is_some()`. Never removing a part and never touching
`ToolUse` means spec 002's orphaned-`tool_result` invariant
(`specs/002-agent-loop/spec.md:249-255`) cannot be violated by construction.

### 6.5 Rendering the Live State (D4)

The original design's rendering seam (a `<skill_state>` block at the once-per-turn
system-prompt/`inject_active_goal` seam, `assembly.rs:2228-2270`, rebuilt once per turn at
`mod.rs:1761`) is **superseded**. Direct evidence it mis-serves per-iteration state: the existing
`current_tool_iteration` field is written every iteration (`tier_loop.rs:2379`) but its only
reader, `BudgetHint` in `rebuild_system_prompt`, runs once before the loop
(`assembly.rs:1177`) — `remaining_tool_calls` shown to the model carries over from the previous
turn's last iteration (M2, a separate pre-existing P3 defect, not fixed by this spec).

**Decision**: render at the rolling-tail seam in `llm_dispatch.rs:96-113`, the verified precedent
for mid-turn `messages[0]`-adjacent mutation: `remove_lsp_messages()` → `push_message(Message::
from_legacy(Role::System, &note_text))` → `recompute_prompt_tokens()`, gated today on
`lsp.drain_notes(...)` returning notes, with an in-code provider-safety argument (`:92-95`) that
this point in the loop has no pending `ToolUse`/`ToolResult` pair, so a `Role::System` insert is
safe across OpenAI/Claude/Ollama. The `<skill_state>` block is a sibling branch at the same seam:
remove the previous `<skill_state>` system message → push the freshly rendered one →
`recompute_prompt_tokens()`.

This is licensed by spec 005's Construction-Time contract
(`specs/005-skills/spec.md:107-110`, #6413): any code path mutating `messages[0]` outside
`push_message`'s incremental accounting must recompute the cached prompt-token count afterward —
live precedent already exists at `skill_reload.rs:363-369`. Neither spec 002 nor spec 005 has an
"Ask First" section, and spec 021 §8's Ask-First list is exactly three items (new
`BudgetAllocation` slot / `CompactionState` transitions / new `zeph-context` Cargo dependency),
none of which this seam touches.

**Cost, accepted as-is (NFR-002)**: `recompute_prompt_tokens` (`utils.rs:287-294`) re-tokenizes
every message, so this is `O(history)` per tool-loop iteration. Unlike the LSP-notes precedent,
which only pays this cost when notes actually exist, `<skill_state>` would otherwise fire on
**every** iteration once a state-declaring skill is active. FR-014's identity-skip (render, then
compare to the currently installed block byte-for-byte, and skip remove/push/recompute if
unchanged) is **required**, not optional, to avoid materially worse per-iteration cost than the
precedent it is modeled on. A per-message token cache invalidated by every in-place mutation
(including § 6.4's clearing) would be a larger, riskier change than the feature itself and is not
required by this spec.

### 6.6 Durable-Replay Gate (D1)

`call_llm_durable` builds its exactly-once fingerprint as
`"llm_call:iter={n}:tokens={cached_prompt_tokens}"` (`tier_loop.rs:2329-2331`);
`crates/zeph-durable/src/handle.rs:661-670` aborts the execution
(`ExecutionStatus::Aborted`) on a fingerprint mismatch during replay. Both § 6.4's clearing and
§ 6.5's re-render mutate `cached_prompt_tokens` mid-loop. Clearing is in-memory while SQLite
retains the full uncleared output, so a crash-resume replay recomputes a different token count
at the same iteration than the one recorded at journal-write time — a structural, deterministic
divergence, not a rare edge case.

**Decision**: skill-state mode is inert for the whole session whenever
`durable_agent_turns_config.is_some()` (`state/mod.rs:1038`), written only at construction by
`AgentBuilder::with_durable_agent_turns` (`builder.rs:2570`, explicitly documented as doing no
I/O) — never mutated at runtime, so the gate cannot flip mid-turn. This is deliberately **not**
`durable_ctx.is_some()`, which is lazily populated inside `call_llm_durable`
(`durable_bootstrap.rs:206-214`) and would let the feature activate before the lazy open and then
go inert after — a mid-turn flip this design must avoid.

Rejected alternative: excluding state-driven mutation from `fp_input` requires a counterfactual
"tokens as if uncleared" figure, but `recompute_prompt_tokens` recomputes the sum wholesale over
`self.msg.messages` — there is no attributable delta to subtract. The only way to net it out
would be changing `zeph-durable`'s divergence contract (spec 064 INV-3) for the benefit of an
opt-in token optimization — worse blast radius than gating the feature off.

A `tracing::warn!` fires once per session the first time a state-declaring skill would otherwise
activate under a durable session (FR-015). An acceptance test must assert no `<skill_state>`
block and no clearing pass runs under a `DurableContext`.

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| No `state:` block in SKILL.md | `SkillMeta.state = None`; no tool registered, no rendering, no clearing — zero-cost path |
| Malformed/oversized `state:` YAML (> 8 KiB or invalid syntax) | Parse returns `None` (warn-not-fail), matching `SkillExtensions` behavior — skill still loads |
| Patch supplies a value failing type/enum/bound validation | Whole patch rejected; prior state unchanged; validation error returned as the tool's observation; deterministic retry, no rollback, no extra LLM call |
| Patch supplies `null` for a key with no declared `default` | Key resets to an empty collection (`[]`/`{}`) rather than an error |
| Two or more active skills declare `state:` in the same turn | One primary is selected deterministically (highest matcher score, or GoSkills entry point); others are logged and excluded from state activation — at most one `SkillExecutionState` per turn |
| A state-declaring skill deactivates (hot-reload removes the `state:` block, or the skill drops out of `active_skill_names`) | Takes effect at the next turn start only, same as all skill activation changes — never mid-turn |
| `durable_agent_turns_config.is_some()` | Feature inert for the whole session; one `tracing::warn!` on first would-be activation |
| First iteration of a new turn, `last_batch_tool_call_ids` still holds the previous turn's ids | Must be pre-empted by FR-009 (clear at turn start, or gate on `iteration > 0`) — otherwise a first-iteration patch could clear a batch it never absorbed |
| `<skill_state>` render is byte-identical to the installed block | Skip remove/push/recompute (FR-014) — no wasted `O(history)` retokenization |
| Session resumed from persisted history (SQLite reload or durable journal replay) | State object rebuilds empty (in-memory only); previously-cleared tool outputs reappear at full size since SQLite/journal retained the uncleared content — benign degradation, not a bug (see § Known Limitations) |
| A `ToolOutput` part with no `tool_use_id` needs matching for clearing | Resolved via `find_preceding_tool_use_id`, walking back to the nearest preceding `ToolUse` in the same message |
| A part already sentinel-prefixed or with `compacted_at.is_some()` is targeted again | Skipped — clearing is idempotent, re-runnable |
| Provider does not strongly enforce the tool's JSON schema (weak small-model constraint) | Validation still runs deterministically post-hoc; the existing tool-loop iteration cap bounds retries — no new unbounded loop |

---

## 8. Required Integration Points

Per CLAUDE.md Development Rules, all of the following are mandatory before this feature's PR:

1. **`config.toml` section**:
   ```toml
   [skills.state]
   enabled = true            # master switch; a per-skill `state:` block is still required to activate
   max_schema_bytes = 8192   # cap on the `state:` frontmatter block before serde_norway parse
   ```
2. **CLI**: `zeph skill state schema <name>` — parses and prints the static `SkillStateSchema`
   for a named skill (validates the `state:` block independent of any running session). The
   *live* `SkillExecutionState` is in-memory/session-scoped only and has no CLI surface — it is
   inspected via the TUI/slash command below.
3. **TUI**: `/state` (or `/skill-state`) command prints the active skill's live
   `SkillExecutionState` as pretty-printed JSON; a spinner status indicator ("Validating skill
   state patch…") is shown while a `skill_state_patch` call is being validated, per the TUI Rules
   requirement that every implicit background operation surface a visible indicator.
4. **`--init` wizard**: a new entry under the skills section — "Enable skill execution state
   (bounded, opt-in per skill)?" — setting `[skills.state] enabled`.
5. **`--migrate-config`**: a new migration step adds `[skills.state]` with defaults to existing
   configs that lack the section (per spec 037's migration mechanism).
6. **Live testing playbook**: `.local/testing/playbooks/skill-execution-state.md` with concrete
   scenarios — happy-path patch/clear cycle, invalid-patch retry, durable-session inertness,
   multi-active-skill primary selection, hot-reload mid-run.
7. **Coverage status**: a new row in `.local/testing/coverage-status.md`, status `Untested`.
8. **Tracing**: every new meaningful-work path carries `tracing::info_span!` under
   `skills.state.*` (parse/validate/merge in `zeph-skills`) or `core.skillstate.*`
   (render/clear/tool-dispatch in `zeph-core`), per NFR-006.

An implementation-time decision not fixed by this spec: whether `skill_state_patch` should be
exempt from the utility-scoring / adversarial-policy gates the way `invoke_skill`/`load_skill`
are (`specs/005-skills/spec.md` § Agent-Invocable Skills). Default posture: no exemption (treat
like any other tool) unless live testing shows the gate suppresses legitimate patches.

---

## 9. Concrete Pilot Skill: `skill-audit`

`.zeph/skills/skill-audit/SKILL.md` is the concrete fitting candidate (D6), selected after
scanning all 27 shipped skills. Its procedure: Step 1 enumerates every skill directory; Step 2
`cat`s each SKILL.md in full and reduces it to a fixed verdict tuple; Steps 3–4 build the final
report **purely from the accumulated verdicts** — the full bodies read in Step 2 are dead weight
in history after their own step. It also satisfies the paper's own stated fit criteria: the
trajectory is not the output, and observations have no delayed relevance.

Finalized schema syntax (building on the design-review sketch):

```yaml
state:
  phase:
    type: enum
    values: [listing, auditing, reporting]
    default: listing
  pending:
    type: list
    items: { type: string }
    default: []
  audited:
    type: list
    items:
      type: object
      fields:
        skill:    { type: string }
        spec:     { type: enum, values: [pass, warn, fail] }
        security: { type: enum, values: [safe, warn, fail] }
        rating:   { type: integer, min: 1, max: 10 }
        reason:   { type: string, max_len: 200 }
    default: []
  fixes:
    type: list
    items: { type: string }
    max_items: 50
    default: []
```

`skill-audit` ships this `state:` block in the same PR as the mechanism (FR-scope, § 1), and is
the subject of the gating `zeph-bench` A/B (§ 10). Secondary candidates, weaker fits (smaller
per-step observations, less benefit): `skill-creator` (gather → name → write → validate),
`rust-agent-handoff`.

---

## 10. Success Criteria

Rollout is gated on SC-010 showing a real improvement — mechanism completion alone is not
sufficient.

| ID | Criterion | Target |
|----|-----------|--------|
| SC-001 | `clear_absorbed_tool_results` unit tests | Idempotency (sentinel/`compacted_at` skip), never-removes-a-part, never-orphans-pairing all covered |
| SC-002 | `find_preceding_tool_use_id` unit tests | Correct id resolution across multi-part messages |
| SC-003 | `parse_frontmatter` regression test | All 27 shipped SKILL.md files produce byte-identical `SkillMeta` output before/after the `NestedMode` generalization |
| SC-004 | Durable-session acceptance test | Under `DurableContext`, no `<skill_state>` block rendered, no clearing pass runs, exactly one `tracing::warn!` per session |
| SC-005 | Tool registration test | `skill_state_patch` present iff a state-declaring skill is active this turn |
| SC-006 | Invalid-patch test | Rejected patch leaves prior state unchanged, returns validation error as observation, issues no extra LLM call |
| SC-007 | Turn-boundary test | `last_batch_tool_call_ids` empty at iteration 0 of a new turn; a patch there cannot clear a prior turn's batch |
| SC-008 | Render identity-skip test | No `push_message`/`recompute_prompt_tokens` call when the rendered block is unchanged from the installed one |
| SC-009 | Multi-active-skill test | Exactly one `SkillExecutionState` exists when 2+ state-declaring skills are active; excluded skills are logged |
| SC-010 | `zeph-bench` A/B gate | `skill-audit` in state mode vs. history mode: cumulative tokens and task success measured within a single continuous session (not across a reload); **includes at least one local-model (Ollama) run**, not cloud-only; rollout gated on this showing improvement |

---

## 11. Key Invariants

### Always (without asking)

- Absent `state:` block ⇒ `SkillMeta.state == None` ⇒ no tool registered, no rendering, no
  clearing — additive at every layer
- `clear_absorbed_tool_results` never removes a `ToolUse`/message pairing — only replaces content
  behind `CLEARED_SENTINEL_PREFIX` or sets `compacted_at`
- `ToolState.last_batch_tool_call_ids` is reset at turn start (or the clearing pass is gated on
  `iteration > 0`) before any patch can reference it
- `<skill_state>` renders only in the volatile, never-cached system-message region — never in
  the cache-stable prefix sealed for prompt-caching
- New `[skills.state]` config keys are `#[serde(default)]`; a `--migrate-config` step exists
- `SkillStateSchema`, `SkillExecutionState`, `StatePatch` compile with zero dependency on
  `zeph-core`
- An invalid patch leaves prior valid state untouched and costs no extra LLM call

### Ask First

- Adding a new `BudgetAllocation` slot for `<skill_state>` instead of riding the existing
  system-prompt allocation (spec 021 §8 — this spec deliberately avoids a new slot)
- Coupling sentinel-clearing to `CompactionState` transitions in any future extension (spec 021
  §8 — this spec keeps them structurally independent)
- Any future repair-LLM path added to patch validation — must expose a `*_provider` config field
  per CLAUDE.md's multi-model design principle

### Never

- NEVER journal `SkillExecutionState` to SQLite or the durable journal — in-memory only for this
  spec (spec 064 INV-1; persistence, if ever added, belongs to `zeph-session`, spec 068)
- NEVER clear a `ToolUse` part, and NEVER orphan a `ToolResult`/`ToolOutput` pairing — spec 002's
  hardest correctness invariant (`spec.md:249-255`)
- NEVER enable skill-state mode (tool registration, rendering, or clearing) when
  `durable_agent_turns_config.is_some()`
- NEVER let the `state:` frontmatter block's indented children leak into `SkillMeta.metadata`
- NEVER assert arXiv:2608.26263's benchmark numbers (16.2× reduction, 54.2% pass rate, etc.) as
  Zeph's own measured results — cite as motivating context only
- NEVER register `skill_state_patch` when no state-declaring skill is active this turn
- NEVER let an invalid-patch retry trigger an extra LLM call or a rollback of previously-merged
  valid state
- NEVER let more than one `SkillExecutionState` object exist for a single turn

---

## 12. Cross-References to Other Compaction and Context Work

- **#5916 (provenance decay)** — sentinel-clearing preserves `metadata.trust_level` on the
  retained message (`tier_loop.rs:1988`), so it is strictly better-behaved than summarization
  with respect to provenance. Cross-reference only; no coupling.
- **#6563 (cache-prefix stability)** — genuine interaction: registering a conditional built-in
  tool (`skill_state_patch`) changes the tool-definition set, and therefore the prompt-cache
  prefix, whenever a state-declaring skill activates. This is a known, explicit cost of the
  feature, not a hidden regression.
- **#6356 (reversible eviction)** — orthogonal; operates post-accumulation on already-large
  history, whereas this mechanism prevents accumulation for the absorbed subset. Neither
  supersedes the other.

---

## 13. Known Limitations

- **In-memory only.** `SkillExecutionState` and the sentinel-cleared message content are both
  process-memory state. SQLite persistence and the durable journal retain the full, uncleared
  tool output. A session resumed from persisted history (reload, durable replay) rebuilds
  `SkillExecutionState` empty and starts again at full context size — this is benign degradation,
  not data loss, but it caps the measurable benefit to a single continuous session. The
  `zeph-bench` methodology (SC-010) must benchmark accordingly, never across a reload.
- **Lower ceiling than the source paper.** Zeph forwards thinking blocks verbatim
  (`specs/002-agent-loop/spec.md:93`) and this spec clears only `ToolOutput`/`ToolResult`
  content — much of arXiv:2608.26263's reported 16.2× reduction comes from discarding
  chain-of-thought, which Zeph does not do. Do not expect or advertise a comparable ratio.
- **Single primary schema per turn.** When multiple active skills declare `state:` blocks, only
  one backs the `skill_state_patch` tool this turn (§ 7); this is a scope boundary, not a defect,
  and revisiting it (e.g., namespaced multi-schema tools) is future work if a real use case
  emerges.

---

## 14. Open Questions

- **OQ8 (non-blocking contract due diligence)**: patch validation must remain synchronous and
  cheap for the life of this feature (NFR-001). If a future revision adds any LLM-backed repair
  path, it must route through `TaskSupervisor` per spec 039 and must not run synchronously in the
  tool loop (spec 002's existing ban on synchronous subgoal extraction is the direct precedent).
  Not a blocker for this spec's MVP (no LLM call exists), but implementers must re-check this
  before adding one.
- **OQ9 (stale-spec landmines, already avoided above)**: `specs/005-skills/spec.md` describes a
  SKILL.md frontmatter with `tools`/`env`/a `## Tools` section (lines ~45-50), a `channels:`
  field (~409-423), and a `HashMap`-based registry (~54-59) — none of which reflect current code
  (the registry is a `Vec`, `registry.rs:85-99`; the frontmatter and channel-allowlist mechanisms
  actually shipped are documented elsewhere in that same spec under different section names).
  This spec cites only verified-current code and does not rely on those stale sections; fixing
  them is out of scope here and tracked separately.

---

## 15. Implementation Tasks

### T001: `zeph-skills` schema types and parser generalization

- Add `SkillStateSchema` type (field grammar per § 5) and `SkillMeta.state: Option<SkillStateSchema>`
- Add the `state:` block textual extractor mirroring `parse_extensions` (8 KiB cap, warn-not-fail)
- Replace `parse_frontmatter`'s `in_metadata: bool` with `nested: Option<NestedMode>`
  (`Collect`/`Skip`) per § 6.1; add the regression test against all 27 shipped SKILL.md files
  (SC-003)
- Update `MERGE_SYSTEM_PROMPT` to preserve an existing `state:` block on self-learning merge
- Dependencies: none

### T002: `StatePatch` validation and merge (pure, `zeph-skills`)

- Implement schema-driven validation (type/enum/bounds/length) and the shallow dictionary-merge
  + null-deletion semantics from § 5
- Unit tests: valid merge, each validation-failure class, null-deletion to default and to empty
  collection
- Dependencies: T001

### T003: `zeph-context::microcompact` clearing helpers

- Implement `clear_absorbed_tool_results` (FR-011) and `find_preceding_tool_use_id` (FR-012)
- Unit tests: idempotency (SC-001), id-matching correctness (SC-002), never-removes-a-part,
  never-orphans-pairing
- Dependencies: none (parallel with T001/T002)

### T004: `zeph-core` runtime state, activation, and durable gate

- Add `SkillExecutionState` field to `SkillState`; write at the `active_skill_names` assignment
  site (turn start only)
- Implement the primary-schema selection rule for multi-active-skill turns (FR-016, SC-009)
- Add `ToolState.last_batch_tool_call_ids`; implement the turn-start clear / `iteration > 0` gate
  (FR-009, SC-007)
- Implement the `durable_agent_turns_config.is_some()` gate (D1) and the once-per-session
  `tracing::warn!`; acceptance test under `DurableContext` (SC-004)
- Dependencies: T001, T002

### T005: `skill_state_patch` built-in tool

- Register/deregister the tool per active-skill-state (FR-004, SC-005)
- Derive the JSON input schema from the active `SkillStateSchema`
- Wire validation (T002) with deterministic-retry-on-failure semantics (FR-006, SC-006)
- Dependencies: T002, T004

### T006: Clearing pass wiring in `process_single_native_turn`

- Insert the post-validation clearing pass between `handle_native_tool_calls`
  (`tier_loop.rs:2466-2470`) and `maybe_summarize_tool_pair` (`:2472`)
- Call `clear_absorbed_tool_results` (T003) targeting `last_batch_tool_call_ids` (T004)
- Integration test verifying ordering relative to `maybe_summarize_tool_pair` and
  `prune_stale_tool_outputs`
- Dependencies: T003, T004, T005

### T007: Rolling-tail rendering at `llm_dispatch.rs`

- Add the `<skill_state>` sibling branch at the seam described in § 6.5 (remove stale → push →
  `recompute_prompt_tokens`)
- Implement the byte-identical skip optimization (FR-014, SC-008)
- Dependencies: T004

### T008: Config, CLI, TUI, `--init`, `--migrate-config`

- `[skills.state]` config section (§ 8.1) with `#[serde(default)]`
- `zeph skill state schema <name>` CLI subcommand (§ 8.2)
- `/state` TUI command + validating-patch spinner indicator (§ 8.3)
- `--init` wizard entry (§ 8.4)
- `--migrate-config` step (§ 8.5)
- Dependencies: T001

### T009: `skill-audit` pilot skill and playbook/coverage rows

- Add the finalized `state:` block (§ 9) to `.zeph/skills/skill-audit/SKILL.md`
- Write `.local/testing/playbooks/skill-execution-state.md`
- Add the coverage-status row (status `Untested`)
- Dependencies: T001, T005, T006, T007

### T010: `zeph-bench` A/B harness (gating)

- Implement the state-mode-vs-history-mode A/B for `skill-audit` (cumulative tokens, task
  success), single continuous session per run, at least one local-model (Ollama) run (SC-010)
- Dependencies: T009

---

## 16. See Also

- [[MOC-specs]] — map of all specifications
- [[constitution]] — project-wide principles
- [[001-system-invariants/spec]] — cross-cutting invariants
- [[002-agent-loop/spec]] — turn lifecycle, orphaned-`tool_result` invariant, thinking-verbatim
  invariant, non-blocking synchronous-extraction ban
- [[005-skills/spec]] — SKILL.md format, `SkillExtensions` sibling pattern, Construction-Time
  contract, GoSkills grouping
- [[021-zeph-context/spec]] — `microcompact`, `BudgetAllocation`, `CompactionState`, Ask-First
  boundary this spec stays clear of
- [[034-zeph-bench/spec]] — benchmark harness used for the SC-010 gating criterion
- [[039-background-task-supervisor/spec]] — supervised background work, relevant if a future
  repair-LLM path is added (OQ8)
- [[064-durable-execution/spec]] — replay fingerprint and INV-1/INV-3 underlying the D1 gate
