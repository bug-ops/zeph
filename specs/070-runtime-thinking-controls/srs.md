---
aliases:
  - Runtime Thinking Controls SRS
  - Think Tokens Reasoning Effort SRS
  - SRS 3098
tags:
  - sdd
  - srs
  - llm
  - core
created: 2026-07-10
status: approved
related:
  - "[[specs/070-runtime-thinking-controls/brd]]"
  - "[[specs/070-runtime-thinking-controls/spec]]"
  - "[[specs/070-runtime-thinking-controls/nfr]]"
---

# SRS: Runtime Thinking Controls — `/think-tokens` and `/reasoning-effort` (GitHub #3098)

ISO/IEC/IEEE 29148:2018 compliant. Requirements use EARS notation. Technical basis: architect
handoff `.local/handoff/2026-07-10T15-50-16-architect.md`, critic-approved
`.local/handoff/2026-07-10T15-58-02-critic.md`.

## 1. Scope

Two new channel-agnostic slash commands, `/think-tokens` and `/reasoning-effort`, mutate the
active LLM provider's thinking/reasoning configuration in place, between turns, for the
duration of the current session only. A companion CLI flag, `--reasoning-effort`, applies the
effort level at startup. This SRS also captures the three correctness/UX fixes (S1, S2, S3)
folded into the architect's revised design.

---

## 2. Command Syntax and Dispatch

### FR-001: `/think-tokens` Command

**WHEN** the user invokes `/think-tokens <arg>`,
**THE SYSTEM SHALL** parse `<arg>` as a token-budget spec via `parse_token_budget`, accepting:
plain integers, case-insensitive `k` suffix (×1,000), case-insensitive `M` suffix
(×1,000,000), one decimal digit (`10.5k` → 10500, rounded to nearest integer), and the literal
`0` or `off` (case-insensitive) meaning "disable thinking".

**WHEN** `<arg>` is empty (no-arg invocation),
**THE SYSTEM SHALL** display the current thinking-token budget for the active provider via a
read-only getter, without mutating state.

**WHEN** `<arg>` fails to parse (negative number, malformed suffix, garbage input),
**THE SYSTEM SHALL** return a descriptive parse-error message and SHALL NOT mutate provider
state.

### FR-002: `/reasoning-effort` Command

**WHEN** the user invokes `/reasoning-effort <low|medium|high>`,
**THE SYSTEM SHALL** parse the argument into a `ReasoningEffort` enum value (`Low | Medium |
High`) and apply it to the active provider.

**WHEN** `<arg>` is empty (no-arg invocation),
**THE SYSTEM SHALL** display the current reasoning-effort level for the active provider via a
read-only getter, without mutating state.

**WHEN** `<arg>` is not one of `low`, `medium`, `high` (case-insensitive),
**THE SYSTEM SHALL** return a descriptive parse-error message and SHALL NOT mutate provider
state.

### FR-003: Channel-Agnostic Registration

**THE SYSTEM SHALL** register both commands once, on the shared `agent_reg` registry
(`crates/zeph-core/src/agent/mod.rs`), so CLI, TUI, and every configured channel adapter
(Telegram/Discord/Slack) dispatch through the identical handler with no per-channel
duplication.

### FR-004: Turn-Boundary Mutation, No Lock

**THE SYSTEM SHALL** apply both commands' mutations to `self.provider` (`AnyProvider`, not
`Arc<dyn>`, not lock-wrapped) at the top of `Agent::run`, strictly between turns, before
`process_user_message` begins. **THE SYSTEM SHALL NOT** introduce a new lock, `RwLock`,
`ArcSwap`, or other synchronization primitive to guard this mutation.

> Rationale: the turn loop is single-threaded `&mut self`; there is no concurrent reader of
> `self.provider` at mutation time. This matches the existing `/provider` switch and
> `set_reasoning_effort` restore-path precedent (architect handoff, "Core mechanism").

---

## 3. Provider Capability Matrix

### FR-005: Per-Provider Support

**THE SYSTEM SHALL** support the two commands per this capability matrix:

| Provider | `/think-tokens N` (token budget) | `/reasoning-effort low\|medium\|high` |
|----------|-----------------------------------|-----------------------------------------|
| Claude | `ThinkingConfig::Extended { budget_tokens }` | `ThinkingConfig::Adaptive { effort }` |
| OpenAI / Compatible | unsupported → warn, no-op | `reasoning_effort` string field (existing contract) |
| Gemini | `thinking_budget: i32` (2.5 models) | `thinking_level` → `GeminiThinkingLevel` (3+ models) |
| Ollama / other | unsupported → warn, no-op | unsupported → warn, no-op |

**WHEN** a command targets a provider/parameter combination marked "unsupported" above,
**THE SYSTEM SHALL** return a message of the form "provider `<name>` does not support
`<capability>`" and **SHALL NOT** silently no-op without any user-visible feedback.

### FR-006: Claude Extended/Adaptive Cross-Override

**WHEN** `/think-tokens` is invoked on a Claude session,
**THE SYSTEM SHALL** set `ThinkingConfig::Extended { budget_tokens }`, overriding any prior
`Adaptive` variant previously set by `/reasoning-effort`.

**WHEN** `/reasoning-effort` is invoked on a Claude session,
**THE SYSTEM SHALL** set `ThinkingConfig::Adaptive { effort }`, overriding any prior `Extended`
variant previously set by `/think-tokens`.

**THE SYSTEM SHALL** surface this cross-override in the confirmation message returned to the
user (e.g. noting that the other Claude thinking mode was replaced).

> Rationale: `Extended` and `Adaptive` are mutually exclusive variants of one `thinking` enum
> field (`zeph-config/src/providers/thinking.rs`); this is intentional, not an oversight.

### FR-007: `/think-tokens off` / `0` Per-Provider Disable Mapping

**WHEN** `/think-tokens off` (or `0`) is invoked on Claude,
**THE SYSTEM SHALL** call `set_thinking(None)`, sending no thinking config on subsequent
requests, and restore `max_tokens` per FR-010 (S1).

**WHEN** `/think-tokens off` (or `0`) is invoked on Gemini,
**THE SYSTEM SHALL** map the disable intent to `Some(0i32)` (Gemini's native disable value),
**NOT** to `None` — `None` would mean "unset → fall back to config default", which could
silently re-enable thinking, the opposite of the user's intent.

**THE SYSTEM SHALL** treat Gemini's `-1` "dynamic" value as unreachable via `/think-tokens` in
this MVP; the config-default path remains the only way to select dynamic thinking. This is a
documented limitation, not a defect.

**WHEN** `/think-tokens N` is invoked on Claude or Gemini with `N` outside the provider's valid
range (Claude: `[1024, 128_000]` and `< max_tokens`; Gemini: `[1, 32_768]`),
**THE SYSTEM SHALL** return an `LlmError`-derived error message and **SHALL NOT** mutate
provider state. There is no panic path.

---

## 4. Correctness and UX Fixes (S1 / S2 / S3)

### FR-008: S1 — Claude `base_max_tokens` Snapshot (Ratchet Fix)

**THE SYSTEM SHALL** add a `base_max_tokens: u32` field to `ClaudeProvider`, written exactly
once in the constructor from the user-configured `max_tokens`, before any `with_thinking`
builder runs, and never mutated thereafter.

**WHEN** `set_thinking(None)` is called (i.e. `/think-tokens off` with no prior
`/reasoning-effort`, or any path that disables Claude thinking),
**THE SYSTEM SHALL** restore `self.max_tokens = self.base_max_tokens` and clear
`self.thinking`.

**WHEN** `set_thinking(Some(_))` is called (either `Extended` or `Adaptive`),
**THE SYSTEM SHALL** recompute the effective `max_tokens` from the immutable
`base_max_tokens` baseline (`base_max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING)`), **NOT** from
the current (possibly already-floored) `self.max_tokens`, guaranteeing idempotency across
repeated enable calls.

**THE SYSTEM SHALL** refactor `with_thinking` to delegate to `set_thinking(Some(thinking))`
so construction-time behavior is preserved byte-for-byte (DRY; no duplicated
validation/flooring logic).

### FR-009: S2 — `/provider` Switch Reset Notice

**WHEN** `handle_provider_switch` is about to replace `self.provider` with a freshly built
provider from the static config pool,
**THE SYSTEM SHALL**, while `self.provider` still references the old instance, capture
`had_reasoning_override = self.provider.current_thinking_budget().is_some() ||
self.provider.current_reasoning_effort().is_some()`.

**WHEN** `had_reasoning_override` is `true`,
**THE SYSTEM SHALL** append a notice to the switch confirmation message stating that
thinking/reasoning-effort settings are per-provider, do not carry over, and that the user must
re-run `/think-tokens` or `/reasoning-effort` for the new provider.

**WHEN** `had_reasoning_override` is `false`,
**THE SYSTEM SHALL NOT** append the notice (avoid noise on every ordinary switch).

**THE SYSTEM SHALL NOT** modify the existing `ProviderOverrides { reasoning_effort:
entry.reasoning_effort.clone() }` construction literal in `handle_provider_switch` — the S2
fix is a warning-message change only, not a persistence change.

> Known non-blocking property (critic-flagged, ship-as-is for MVP): `had_reasoning_override`
> returns `true` for ANY active thinking config, including one set purely from static
> `config.toml` defaults with no runtime command ever invoked this session. The resulting
> notice is conservative over-warning, not a correctness defect — the new provider genuinely
> does use different defaults. A future "runtime override was set this session" dirty flag is
> an optional follow-up, not required for this spec.

### FR-010: S3 — Session-Only, No Persistence

**THE SYSTEM SHALL NOT** add any field to `ProviderOverrides` (`zeph-config`) for
`/think-tokens` or `/reasoning-effort`.

**THE SYSTEM SHALL NOT** modify `restore_provider_overrides`
(`provider_cmd.rs:113-197`) to fan out to Claude or Gemini.

**THE SYSTEM SHALL NOT** persist any `/think-tokens` or `/reasoning-effort` value to SQLite.

**THE SYSTEM SHALL** leave the existing `set_reasoning_effort(&mut self, Option<String>)`
OpenAI-only restore-path method entirely unchanged; the new `apply_reasoning_effort(&mut self,
ReasoningEffort)` fan-out method on `AnyProvider` is a separate, parallel method used only by
the new live commands.

**Consequence:** `--migrate-config` requires no new migration step for this feature (no
config.toml parameter added, no SQLite schema/blob change).

---

## 5. CLI Flag

### FR-011: `--reasoning-effort` Startup Flag

**WHEN** the binary is started with `--reasoning-effort <low|medium|high>`,
**THE SYSTEM SHALL** apply that effort level, before the first turn, to every configured
provider entry that supports a reasoning-effort concept (Claude → `Adaptive` effort,
OpenAI/Compatible → `reasoning_effort` field, Gemini → `thinking_level`), fanning out next to
the existing `--thinking` application block (`src/runner.rs` ~L1282-1289).

**THE SYSTEM SHALL NOT** add a `--think-tokens` startup flag (M2) — the existing
`--thinking extended:N` flag remains the Claude-only startup token-budget mechanism; there is
no clean cross-provider startup semantics for a generic token-budget flag, and the runtime
`/think-tokens` command already covers the mid-session case for every provider.

### FR-012: `--init` Wizard Prompt

**WHEN** the `--init` interactive wizard configures a Claude, OpenAI, or Gemini provider,
**THE SYSTEM SHALL** prompt for a default reasoning-effort value (the thinking-budget default
already exists via the provider's `thinking`/`thinking_budget` config fields and needs no new
prompt).

---

## 6. Display and Feedback

### FR-013: No-Arg Display Path

**WHEN** either command is invoked with no argument,
**THE SYSTEM SHALL** format and return the current setting using the read-only getters
`current_thinking_budget(&self) -> Option<u32>` and `current_reasoning_effort(&self) ->
Option<String>` on `AnyProvider`, without touching provider state.

### FR-014: Instant Feedback, No Spinner

**THE SYSTEM SHALL** return a confirmation or error string synchronously from the handler
(`Future<Output = String>` at the `AgentAccess` boundary, matching the existing
`handle_caveman`/`handle_model` pattern). **THE SYSTEM SHALL NOT** display a background-status
spinner for either command — the mutation is a local, synchronous, non-I/O operation, so the
TUI "user must always know what is happening" rule (CLAUDE.md, TUI Rules) is satisfied by the
returned confirmation text itself, not a spinner.

---

## 7. Deferred Requirements (Acknowledged)

### FR-D-01: `--think-tokens` Startup Flag

Deferred (see BRD §5, M2). No cross-provider startup token-budget semantics exists; the
runtime `/think-tokens` command is the intended mid-session mechanism for all providers.

### FR-D-02: OpenAI `minimal` Reasoning-Effort Value

Deferred (see BRD §5, M3). MVP keeps the existing 3-value contract. Follow-up issue candidate,
not filed as part of this spec's implementation.

### FR-D-03: S2 Dirty-Flag Refinement

Deferred (see BRD §5). The conservative over-warn on config-default thinking is acceptable for
MVP; a "runtime override set this session" flag is optional future work.

---

## 8. Traceability Matrix

| Requirement | BRD Goal | Architect/Critic Source |
|-------------|----------|--------------------------|
| FR-001, FR-002 | BG-01 | Architect §"Runtime mutation surface to ADD" — Command layer |
| FR-003 | BG-05 | Architect §"Agent layer" — `mod.rs` registration |
| FR-004 | BG-01, BG-02 | Architect §"Core mechanism (confirmed sound)"; critic re-verification |
| FR-005, FR-006, FR-007 | BG-01, BG-03 | Architect §"The two commands are genuinely different knobs"; M1 |
| FR-008 | BG-01 | Architect/critic S1 — `base_max_tokens` snapshot |
| FR-009 | BG-04 | Architect/critic S2 — switch reset notice |
| FR-010 | (scope boundary) | Architect/critic S3 — session-only |
| FR-011, FR-012 | BG-01 | Architect §"Binary (`src/`)"; M2 |
| FR-013, FR-014 | BG-02 | Architect §"Agent layer" — `agent_access_impl.rs`; CLAUDE.md TUI Rules |
| FR-D-01..03 | (deferred) | BRD §5, M2/M3, critic non-blocking minor |
