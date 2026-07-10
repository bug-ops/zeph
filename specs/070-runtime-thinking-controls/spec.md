---
aliases:
  - Runtime Thinking Controls
  - Think Tokens Reasoning Effort Spec
  - Spec 3098
tags:
  - sdd
  - spec
  - llm
  - core
created: 2026-07-10
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[specs/070-runtime-thinking-controls/brd]]"
  - "[[specs/070-runtime-thinking-controls/srs]]"
  - "[[specs/070-runtime-thinking-controls/nfr]]"
  - "[[specs/070-runtime-thinking-controls/plan]]"
  - "[[specs/003-llm-providers/spec]]"
  - "[[specs/042-zeph-commands/spec]]"
  - "[[specs/002-agent-loop/spec]]"
---

# Spec: Runtime Thinking Controls — `/think-tokens` and `/reasoning-effort` (GitHub #3098)

> [!info]
> Aider-style runtime slash commands that mutate the active LLM provider's thinking-token
> budget or reasoning-effort level mid-session, in place, with no restart and no persistence.
> This spec is the authoritative implementation contract, derived verbatim from the
> critic-approved architect design at `.local/handoff/2026-07-10T15-50-16-architect.md`
> (parent: `.local/handoff/2026-07-10T15-58-02-critic.md`, verdict **APPROVED**). This spec
> formalizes that design into traceable requirements; it does not re-derive the architecture.

## Sources

### External
- [Aider documentation — chat modes and settings](https://aider.chat/) — runtime reasoning-effort/thinking-budget adjustment without restart (competitive-parity reference, `.claude/rules/continuous-improvement.md`)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-llm/src/claude/mod.rs` | `ClaudeProvider`, `with_thinking`/`set_thinking`, `base_max_tokens` snapshot (S1) |
| `crates/zeph-llm/src/gemini/mod.rs` | `GeminiProvider`, `with_thinking_budget`/`with_thinking_level` builders and new runtime setters |
| `crates/zeph-llm/src/any.rs` | `AnyProvider` enum fan-out, `ReasoningEffort` enum, `set_thinking_budget`/`apply_reasoning_effort`/getters |
| `crates/zeph-llm/src/openai/mod.rs` | `OpenAiProvider::set_reasoning_effort` (existing, OpenAI-only, string-based, restore-path only) |
| `crates/zeph-config/src/providers/thinking.rs` | `ThinkingConfig::{Extended, Adaptive}` mutually-exclusive enum |
| `crates/zeph-commands/src/handlers/model.rs` | Template `CommandHandler` for the two new handlers |
| `crates/zeph-commands/src/traits/agent.rs` | `AgentAccess` fat trait; new `handle_think_tokens`/`handle_reasoning_effort` methods |
| `crates/zeph-commands/src/commands.rs` | Static `CommandInfo` registry (drives `/help`; TUI slash-autocomplete uses a separate `zeph-tui` registry not wired to this one) |
| `crates/zeph-core/src/agent/agent_access_impl.rs` | `Agent<C>` implementations of the two handlers; template `handle_caveman` (~L843-875) |
| `crates/zeph-core/src/agent/provider_cmd.rs` | `handle_provider_switch`, `build_switch_message` — S2 extension point |
| `crates/zeph-core/src/agent/mod.rs` | `provider: AnyProvider` field (:161); `agent_reg` registration (~L592-661) |
| `src/cli.rs` | New `--reasoning-effort <low\|medium\|high>` flag |
| `src/runner.rs` | Existing `--thinking` startup application block (~L1282-1289); new fan-out for `--reasoning-effort` |
| `src/init/mod.rs` | `--init` wizard; new reasoning-effort default prompt |

---

## 1. Overview

### Problem Statement

Zeph's LLM providers expose reasoning-depth knobs (Claude extended/adaptive thinking, OpenAI
`reasoning_effort`, Gemini `thinking_budget`/`thinking_level`) only as static `config.toml`
values or a one-shot startup CLI flag. Changing them requires a full restart, discarding the
in-memory session. This is a UX gap relative to Aider, a tracked competitive-parity reference
agent, which allows this tuning at runtime.

### Goal

Two channel-agnostic slash commands — `/think-tokens [N|Nk|NM|off]` and `/reasoning-effort
[low|medium|high]` — mutate the active provider's in-memory configuration between turns, take
effect on the very next turn, are visible via a no-arg display path, and are session-scoped
only (never persisted). A companion `--reasoning-effort` CLI flag applies the effort level at
startup.

### Out of Scope

- Persisting either setting across process restart or `/provider` switch (S3 — see §4 below)
- A startup `--think-tokens` flag (M2 — `--thinking extended:N` remains the Claude-only startup
  token-budget mechanism)
- OpenAI's `minimal` reasoning-effort value (M3 — MVP keeps the existing 3-value contract)
- A "runtime override set this session" dirty flag to tighten the S2 reset-notice trigger
  (non-blocking critic minor; conservative over-warning is acceptable for MVP)

Full requirement-level detail: `[[specs/070-runtime-thinking-controls/srs]]`. Quality targets:
`[[specs/070-runtime-thinking-controls/nfr]]`.

---

## 2. Functional Requirements

See `[[specs/070-runtime-thinking-controls/srs]]` for the complete EARS-notation requirement
set (FR-001 through FR-014) and traceability matrix. Summary:

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001/002 | `/think-tokens` and `/reasoning-effort` parse, validate, no-arg display, mutate | must |
| FR-003 | Single channel-agnostic registration on `agent_reg` | must |
| FR-004 | Turn-boundary mutation on `&mut self`, no new lock | must |
| FR-005/006/007 | Provider capability matrix, Claude cross-override, per-provider disable mapping | must |
| FR-008 | S1 — `base_max_tokens` snapshot, ratchet-free restore | must |
| FR-009 | S2 — `/provider` switch reset notice | must |
| FR-010 | S3 — strictly session-only, no persistence | must |
| FR-011/012 | `--reasoning-effort` CLI flag + `--init` wizard prompt | must |
| FR-013/014 | Read-only display path; synchronous confirmation, no spinner | must |

---

## 3. Architecture / Design

### 3.1 Core Mechanism (unchanged, confirmed sound)

The agent owns the provider inline as `provider: AnyProvider` (an enum, not `Arc<dyn>`, not
lock-wrapped) at `crates/zeph-core/src/agent/mod.rs:161`. Slash commands dispatch at the top of
`Agent::run`, between turns, before `process_user_message`. The turn loop is single-threaded
`&mut self` — there is no concurrent reader of `self.provider` at mutation time. Mutating
`self.provider` in a slash handler is therefore race-free, identical to the existing
`/provider` switch and `set_reasoning_effort` restore path.

LLM request methods (`chat`, `chat_with_tools`, …) take only `&[Message]` (+
`&[ToolDefinition]`) — there is no per-request options struct — so the knob must live on the
provider instance. In-place mutation is the only precedent-matching approach; ArcSwap/RwLock
alternatives are correctly rejected as adding a lock on the hot path for a value that only
changes at turn boundaries.

### 3.2 Two Distinct Knobs

`/think-tokens` is a **token-budget** knob; `/reasoning-effort` is an **effort-level**
(low/medium/high) knob. See the capability matrix in SRS §3 (FR-005). Claude's `Extended` and
`Adaptive` are mutually-exclusive variants of one `thinking` enum — setting one via either
command overrides the other, and this cross-override is surfaced in the confirmation message
(FR-006).

### 3.3 Runtime Mutation Surface (13 files)

| # | File | Change |
|---|------|--------|
| 1 | `crates/zeph-llm/src/claude/mod.rs` | `base_max_tokens: u32` field + constructor init; `set_thinking(&mut self, Option<ThinkingConfig>) -> Result<(), LlmError>` (S1 restore-on-None); `with_thinking` delegates to it; `current_thinking_budget`/`current_reasoning_effort` getters |
| 2 | `crates/zeph-llm/src/gemini/mod.rs` | `set_thinking_budget(&mut self, Option<i32>)`, `set_thinking_level(&mut self, Option<GeminiThinkingLevel>)` + current getters, mirroring existing consuming builders |
| 3 | `crates/zeph-llm/src/any.rs` | `set_thinking_budget(Option<u32>)`, `apply_reasoning_effort(ReasoningEffort)`, `current_thinking_budget`, `current_reasoning_effort`, `ReasoningEffort` enum; cover `Masked(inner)` arm; existing `set_reasoning_effort(Option<String>)` unchanged |
| 4 | `crates/zeph-commands/src/handlers/think_tokens.rs` + `reasoning_effort.rs` | New `CommandHandler` structs (template `handlers/model.rs`); `parse_token_budget` + unit tests |
| 5 | `crates/zeph-commands/src/handlers/mod.rs` | `pub mod` lines |
| 6 | `crates/zeph-commands/src/traits/agent.rs` | Two `AgentAccess` methods + two `NullAgent` stubs |
| 7 | `crates/zeph-commands/src/commands.rs` | Two `CommandInfo` entries (Configuration block) |
| 8 | `crates/zeph-core/src/agent/agent_access_impl.rs` | `handle_think_tokens`/`handle_reasoning_effort` impl on `Agent<C>` (parse → capability check → provider setter → format) |
| 9 | `crates/zeph-core/src/agent/provider_cmd.rs` | S2 only: capture `had_reasoning_override` before `set_provider`; extend `build_switch_message` signature + reset notice |
| 10 | `crates/zeph-core/src/agent/mod.rs` | Register both commands on `agent_reg` |
| 11 | `src/cli.rs` | `--reasoning-effort <low\|medium\|high>` (no `--think-tokens`) |
| 12 | `src/runner.rs` | Apply `cli.reasoning_effort` at startup near the `--thinking` block; fan out to applicable providers |
| 13 | `src/init/mod.rs` | Wizard reasoning-effort default prompt |

`crates/zeph-config/src/providers/entry.rs` is explicitly **removed from scope** (S3 — no
`ProviderOverrides`/SQLite change). Full step-by-step detail:
`[[specs/070-runtime-thinking-controls/plan]]`; ordered developer tasks:
`[[specs/070-runtime-thinking-controls/tasks]]`.

---

## 4. Key Invariants

### Always (without asking)

- **`base_max_tokens` is captured exactly once**, in `ClaudeProvider::new`, before any
  `with_thinking` builder runs, and is never mutated afterward (S1).
- **`set_thinking(Some(_))` recomputes `max_tokens` from the immutable `base_max_tokens`
  baseline**, not from the current (possibly already-floored) `max_tokens` — every enable call
  is idempotent regardless of prior state (S1).
- **`set_thinking(None)` restores `max_tokens = base_max_tokens`** exactly — the value the user
  originally configured, never the 16k thinking floor (S1).
- **`with_thinking` delegates to `set_thinking`** — one source of truth for range/flooring
  validation (NFR-MA-01).
- **Every unsupported provider/parameter combination returns an explicit "not supported by
  provider X" message** — never a silent no-op (FR-005).
- **The `/provider` switch captures `had_reasoning_override` while `self.provider` still
  references the OLD instance**, before `set_provider` runs (S2).
- **The reset notice appends to `build_switch_message`'s output only when
  `had_reasoning_override` is true** — no notice on ordinary switches with no active override
  (S2, NFR-US-04).
- **Both commands mutate `self.provider` only between turns**, at the top of `Agent::run`,
  before `process_user_message` (FR-004).
- **`ReasoningEffort` lives in `zeph-llm`**, the crate that owns `AnyProvider` — lower layers
  own the domain type (NFR-MA-03).
- **Every new `match` on `AnyProvider` covers the `Masked(inner)` arm**, dispatching to `inner`.

### Ask First

- Widening the `/reasoning-effort` value set beyond `low|medium|high` (e.g. adding `minimal`)
  — deferred per M3, requires an explicit decision to re-open scope.
- Adding a `--think-tokens` startup CLI flag — deferred per M2; would need a cross-provider
  startup semantics design that does not currently exist.
- Tightening the S2 `had_reasoning_override` check with a session dirty-flag — optional,
  non-blocking; requires weighing the added state against the marginal UX benefit.
- Extending `ProviderOverrides` or `restore_provider_overrides` to cover thinking/reasoning
  values — this would re-open the S3 scope boundary and requires an explicit architectural
  decision, not an incidental follow-up commit.

### Never

- **NEVER let a runtime `/think-tokens off` (or the equivalent `None` disable path) leave
  Claude's `max_tokens` inflated at the 16k thinking floor.** This is the S1 regression this
  spec exists to prevent — a naive reuse of the construction-time `with_thinking` body for a
  runtime setter only ever raises `max_tokens`, never lowers it, causing an enable→disable→
  re-enable ratchet that permanently loses the user's configured value.
- **NEVER silently reset or discard a user's active `/think-tokens`/`/reasoning-effort`
  override on a `/provider` switch without telling the user.** The old provider instance is
  simply replaced by a freshly built one from the static config pool; any runtime override on
  the old instance is gone. S2 requires this to be surfaced, not silent.
- **NEVER persist `/think-tokens` or `/reasoning-effort` values to SQLite, `ProviderOverrides`,
  or any other durable store in this iteration.** This is a strict scope boundary (S3), not an
  oversight — the issue explicitly specifies "session only, not persisted", and touching
  `ProviderOverrides` would break three existing construction-literal call sites and require
  extending the restore path with fan-out logic that is deliberately out of scope.
- **NEVER introduce a new lock, `RwLock`, `Mutex`, or `ArcSwap` to guard the provider
  mutation.** The turn-boundary, single-threaded `&mut self` mutation is already race-free;
  adding synchronization here would add hot-path cost for no correctness benefit.
- **NEVER map Gemini's `/think-tokens off` disable intent to `None`.** `None` means "unset,
  fall back to config default", which could silently re-enable thinking — the opposite of the
  user's request. The correct mapping is `Some(0i32)`, Gemini's native disable value (M1).
- **NEVER let an out-of-range token budget or malformed effort string panic.** All validation
  returns `Result`/`LlmError`, formatted into a returned error string; there is no panic path
  in the command→agent→provider chain.
- **NEVER route the persistence-restore path (`set_reasoning_effort`, OpenAI-only,
  string-based) through the new `apply_reasoning_effort` fan-out**, or vice versa. They are
  deliberately separate methods; merging them would blur the S3 scope boundary.

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `/think-tokens` with no argument | Display current budget via `current_thinking_budget()`; no mutation (FR-013) |
| `/think-tokens off` after `/think-tokens 8k` on Claude | `max_tokens` restored to the original config value, not 16k (S1, FR-008) |
| `/think-tokens 8k` → `/think-tokens 16k` → `/think-tokens off` → `/think-tokens 4k` on Claude | Every `Some(_)` step computes from `base_max_tokens`, not from a prior floored value; fully idempotent (S1) |
| `/think-tokens 500` on Claude (below `base_max_tokens` floor after clamping) | `eff = base_max_tokens.max(16000)`; budget validated `< eff`; construction-parity edge case (critic-verified) |
| `/reasoning-effort high` then `/think-tokens 8k` on Claude | `Extended{8k}` overrides the prior `Adaptive{High}`; confirmation message states the override (FR-006) |
| `/think-tokens off` on Gemini | Maps to `Some(0i32)`, not `None` (M1, never silently re-enables) |
| `/think-tokens -1` / `/think-tokens 1.2.3k` / garbage input | Descriptive parse error; no mutation |
| `/reasoning-effort` on Ollama | "provider `ollama` does not support reasoning effort" — explicit message, no silent no-op (FR-005) |
| `/think-tokens 8k` on OpenAI/Compatible | "provider `<name>` does not support a thinking-token budget" |
| `/provider claude` switch after `/reasoning-effort high` was set on the old provider | Switch confirmation includes the reset notice (S2, FR-009) |
| `/provider claude` switch with no runtime override ever set this session | No reset notice appended (avoids noise) |
| `/provider` switch away from a provider with only a `config.toml`-default thinking setting (no runtime command ever run) | Reset notice still fires (documented conservative over-warn, non-blocking per critic) |
| `--reasoning-effort high` at startup | Applied to every provider entry that supports it, before the first turn (FR-011) |
| Background task holding a `self.provider.clone()` snapshot taken before a runtime mutation | Snapshot is stale by design; consistent with existing `/provider`/`reasoning_effort` behavior, not a bug (NFR-CC-03) |

---

## 6. Success Criteria

See `[[specs/070-runtime-thinking-controls/brd]]` §6 (SC-01 through SC-08) for the full
business-facing success criteria. Implementation-facing checklist:

- [ ] All 13 files listed in §3.3 are implemented per this spec and the plan
- [ ] S1 regression test passes: Claude enable → off → re-enable restores original `max_tokens`
- [ ] S2 reset notice fires exactly when a runtime override was active, and only then
- [ ] S3 verified: `git diff` shows zero changes to `crates/zeph-config/src/providers/entry.rs`
      and zero new `ProviderOverrides` fields
- [ ] Provider capability matrix (SRS §3) verified live across Claude, OpenAI, Gemini, Ollama
- [ ] `cargo +nightly fmt --check`, `cargo clippy --profile ci ... -D warnings`, `cargo nextest
      run ...`, and the rustdoc gate all pass per `.claude/rules/branching.md`
- [ ] `.local/testing/playbooks/thinking-reasoning-runtime.md` created with scenarios per the
      architect handoff §"Testing playbook"
- [ ] `.local/testing/coverage-status.md` rows added for `/think-tokens` and
      `/reasoning-effort` (status `Untested`)
- [ ] `specs/003-llm-providers/spec.md` carries the one-line `&self` invariant clarification

---

## 7. Relationship to Existing Specs

| This spec | Existing spec | Relationship |
|-----------|---------------|--------------|
| Provider setters, `ReasoningEffort`, `AnyProvider` fan-out | `[[specs/003-llm-providers/spec]]` | Extends `AnyProvider`; clarifies the `&self` invariant to scope to request/inference methods only, exempting turn-boundary config setters (see spec-003 addendum below) |
| Two new `CommandHandler`s, `AgentAccess` methods | `[[specs/042-zeph-commands/spec]]` | New entries in the static `CommandInfo` registry; no change to the registry's dispatch mechanism |
| Turn-boundary dispatch timing | `[[specs/002-agent-loop/spec]]` | Mutation point is the existing pre-turn slash-command dispatch phase; no new phase introduced |
| S3 non-persistence boundary | `[[specs/065-ephemeral-plugins-provider-overrides/spec]]` | That spec added `ProviderOverrides.reasoning_effort` for the OpenAI-only restore path; this spec deliberately does NOT extend it further |

### spec-003 Addendum (applied as part of this spec's implementation)

`specs/003-llm-providers/spec.md` §"Key Invariants" states "Provider methods are always
`&self` — immutable, concurrent-safe". This scopes to **request/inference** methods (`chat`,
`chat_stream`, `chat_with_tools`, …), which remain `&self`. The `&mut self` config setters
introduced by this spec (`set_thinking`, `set_thinking_budget`, `set_thinking_level`,
`apply_reasoning_effort`), applied only at turn boundaries between requests, are exempt —
`set_reasoning_effort` already established this pattern before the invariant was written. A
one-line clarification is added to spec-003 accordingly (see the diff applied alongside this
spec package).

---

## 8. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[specs/070-runtime-thinking-controls/brd]] — Business case and success criteria
- [[specs/070-runtime-thinking-controls/srs]] — Full functional requirements (EARS)
- [[specs/070-runtime-thinking-controls/nfr]] — Quality targets (ISO/IEC 25010)
- [[specs/070-runtime-thinking-controls/plan]] — Step-by-step implementation plan
- [[specs/070-runtime-thinking-controls/tasks]] — Ordered developer task breakdown
- [[specs/003-llm-providers/spec]] — `LlmProvider`/`AnyProvider` contract, `&self` invariant
- [[specs/042-zeph-commands/spec]] — Slash command registry and dispatch
- [[specs/002-agent-loop/spec]] — Turn lifecycle and pre-turn dispatch phase
