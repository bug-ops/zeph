---
aliases:
  - Runtime Thinking Controls Plan
  - Think Tokens Reasoning Effort Plan
  - Plan 3098
tags:
  - sdd
  - plan
  - llm
  - core
created: 2026-07-10
status: approved
related:
  - "[[specs/070-runtime-thinking-controls/spec]]"
  - "[[specs/070-runtime-thinking-controls/tasks]]"
---

# Implementation Plan: Runtime Thinking Controls — `/think-tokens` and `/reasoning-effort` (GitHub #3098)

Source of truth: architect handoff `.local/handoff/2026-07-10T15-50-16-architect.md`
(critic-approved, `.local/handoff/2026-07-10T15-58-02-critic.md`, verdict **APPROVED**). This
plan sequences the 13-file change set from `[[specs/070-runtime-thinking-controls/spec]]` §3.3
into an implementable order. No architectural re-derivation — this is a formalization of the
already-approved design.

## Recommended Implementation Order

**Phase 1: `zeph-llm` provider layer (S1 fix + runtime setters/getters)** — implement first.

Rationale:
- Self-contained within `zeph-llm`; no dependency on command/agent layers
- S1 is a correctness bug fix (max_tokens ratchet) — land and unit-test it before anything
  depends on `set_thinking`
- `AnyProvider` fan-out is the single dispatch surface every higher layer calls into

**Phase 2: `zeph-commands` command layer** — implement second.

Rationale:
- Depends on `ReasoningEffort` (Phase 1) and the parser it needs to produce
- No dependency on `zeph-core`; can be unit-tested in isolation with `NullAgent` stubs

**Phase 3: `zeph-core` agent layer (handlers + S2 fix + registration)** — implement third.

Rationale:
- Depends on both the `AnyProvider` fan-out (Phase 1) and the `AgentAccess` trait extension
  (Phase 2)
- S2 fix (`provider_cmd.rs`) is independent of the new commands but touches the same file
  family — bundling it here keeps the `zeph-core` changes in one review pass

**Phase 4: Binary (`src/`) — CLI flag, startup wiring, wizard** — implement last.

Rationale:
- Depends on the full stack being in place (`AnyProvider::apply_reasoning_effort` etc.)
- Lowest risk, most mechanical phase

---

## Phase 1: `zeph-llm` Provider Layer

### P1-1: Claude `base_max_tokens` snapshot and `set_thinking` (S1)

**File:** `crates/zeph-llm/src/claude/mod.rs`

1. Add field `base_max_tokens: u32` to `ClaudeProvider`.
2. In the constructor (`ClaudeProvider::new(api_key, model, max_tokens: u32)`, :205), add
   `base_max_tokens: max_tokens` to the `Self { .. }` initializer (:213-217) — captured before
   any `with_thinking` builder chains.
3. Add `pub fn set_thinking(&mut self, thinking: Option<ThinkingConfig>) -> Result<(),
   LlmError>`:
   - `None` → `self.max_tokens = self.base_max_tokens; self.thinking = None; Ok(())`
   - `Some(Extended { budget_tokens })` → validate `budget_tokens ∈ [1024, 128_000]`; compute
     `let eff = self.base_max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING)`; validate
     `budget_tokens < eff`; `self.max_tokens = eff; self.thinking = Some(thinking); Ok(())`
   - `Some(Adaptive { .. })` (or any non-`Extended` variant) →
     `self.max_tokens = self.base_max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING);
     self.thinking = Some(thinking); Ok(())`
4. Refactor `with_thinking(mut self, thinking) -> Result<Self, LlmError>` to delegate:
   `self.set_thinking(Some(thinking))?; Ok(self)`. Extract shared range/`<` validation into a
   private helper only if it improves readability — delegation alone removes the duplication.
5. `with_thinking_opt` (:464) is unaffected — it already forwards to `with_thinking`.
6. Add getters: `current_thinking_budget(&self) -> Option<u32>` (returns
   `Extended.budget_tokens` when `thinking` is `Some(Extended{..})`, else `None`) and
   `current_reasoning_effort(&self) -> Option<String>` (returns `Adaptive.effort` normalized to
   a string when `thinking` is `Some(Adaptive{..})`, else `None`).

### P1-2: Gemini runtime setters

**File:** `crates/zeph-llm/src/gemini/mod.rs`

Mirror the existing consuming builders `with_thinking_budget` (~:98) and `with_thinking_level`
(~:110):
- `set_thinking_budget(&mut self, Option<i32>) -> Result<(), LlmError>`
- `set_thinking_level(&mut self, Option<GeminiThinkingLevel>)`
- Getters for the current budget / level

### P1-3: `AnyProvider` fan-out

**File:** `crates/zeph-llm/src/any.rs`

Add:
- `ReasoningEffort { Low, Medium, High }` enum with `as_str`/`FromStr`
- `set_thinking_budget(&mut self, Option<u32>) -> Result<(), LlmError>` — `None` = disable,
  mapped per-provider (M1: Claude → `set_thinking(None)`; Gemini → `Some(0i32)`, NOT `None`;
  OpenAI/Compatible/Ollama → NotSupported result)
- `apply_reasoning_effort(&mut self, effort: ReasoningEffort) -> Result<(), LlmError>` —
  Claude → `set_thinking(Some(Adaptive{effort}))`; OpenAI/Compatible → existing
  `reasoning_effort` string field; Gemini → `set_thinking_level(Some(level))`; others →
  NotSupported
- `current_thinking_budget(&self) -> Option<u32>` and `current_reasoning_effort(&self) ->
  Option<String>` (display path + S2 had-override check)
- Cover the `Masked(inner)` arm in every new match
- **Leave `set_reasoning_effort(&mut self, Option<String>)` (OpenAI-only, restore path)
  unchanged** — the new `apply_reasoning_effort` is a separate, parallel method (S3 boundary)

### P1-4: Unit tests (Phase 1)

- Claude: enable→off→enable idempotency (S1 regression); out-of-range budget rejection;
  construction-parity (base < 1024 edge case)
- Gemini: `off` maps to `Some(0)` not `None`; out-of-range rejection
- `AnyProvider`: fan-out dispatch per provider variant, including `Masked`; unsupported-provider
  NotSupported results

---

## Phase 2: `zeph-commands` Command Layer

### P2-1: New handler modules

**Files:** `crates/zeph-commands/src/handlers/think_tokens.rs`,
`crates/zeph-commands/src/handlers/reasoning_effort.rs`

New `CommandHandler` structs, template `handlers/model.rs`. Add `./.github/scripts/add-spdx-headers.sh`
to both new files before commit.

Put the pure parser here (or in `handlers/mod.rs`):

```
parse_token_budget(&str) -> Result<Option<u32>, String>
```

Rules: case-insensitive `k` = ×1000, `M` = ×1_000_000; accept one decimal (`10.5k` → 10500,
round to nearest int); `0`/`off` (case-insensitive) → `None`; reject negatives and garbage with
a descriptive `Err`.

Unit-test compound/edge inputs: empty, `k`, `-1`, `1.2.3k`, `off`, `0`, `8k`, `10.5k`, `1M`, and
an overflow-sized value. Do not entangle with `runner.rs`'s `parse_thinking_arg` (raw integers
only — different contract).

### P2-2: Module registration

**File:** `crates/zeph-commands/src/handlers/mod.rs`

`pub mod think_tokens; pub mod reasoning_effort;`

### P2-3: `AgentAccess` trait extension

**File:** `crates/zeph-commands/src/traits/agent.rs`

Two new trait methods (~L172-189) + two `NullAgent` stubs (~L671-690).

### P2-4: Command registry entries

**File:** `crates/zeph-commands/src/commands.rs`

Two `CommandInfo` entries in the `// --- Configuration ---` block (drives `/help`; TUI
slash-autocomplete uses a separate `zeph-tui` registry not wired to this one — a pre-existing
gap affecting all `AgentAccess` commands, not specific to this feature).

---

## Phase 3: `zeph-core` Agent Layer

### P3-1: Handler implementations

**File:** `crates/zeph-core/src/agent/agent_access_impl.rs`

Implement `handle_think_tokens` and `handle_reasoning_effort` on `Agent<C>` (template
`handle_caveman`, ~L843-875). Body per handler:

1. Empty arg → read the getter, format and return the current setting.
2. Non-empty → parse/validate (`parse_token_budget` for tokens; `ReasoningEffort::from_str` for
   effort). Parse error → formatted error string.
3. Capability check via the active provider's `ProviderKind` (reuse `provider_cmd.rs:169-180`).
   Unsupported → "provider X does not support …".
4. Call `self.provider.set_thinking_budget(...)` / `self.provider.apply_reasoning_effort(...)`.
   Map `Result` into a confirmation string (include the Claude Extended↔Adaptive cross-override
   note where applicable) or an error string. Both methods are infallible
   `Future<Output = String>` at the `AgentAccess` boundary.

### P3-2: S2 — `/provider` switch reset notice

**File:** `crates/zeph-core/src/agent/provider_cmd.rs`

1. In `handle_provider_switch`, before `self.set_provider(new_provider)` (:466), capture:
   ```rust
   let had_reasoning_override =
       self.provider.current_thinking_budget().is_some()
       || self.provider.current_reasoning_effort().is_some();
   ```
2. Change `build_switch_message` (:501) signature to `fn build_switch_message(&self,
   configured_name: &str, had_reasoning_override: bool) -> String`; pass the captured flag at
   the call site (:494).
3. When `had_reasoning_override` is true, append to both branches of `build_switch_message`:
   > `Note: thinking / reasoning-effort settings are per-provider and do not carry over.
   > '{configured_name}' now uses its configured defaults — re-run /think-tokens or
   > /reasoning-effort to set them for this provider.`
4. Leave the existing `ProviderOverrides { reasoning_effort: entry.reasoning_effort.clone() }`
   literal (:488-490) exactly as-is — no `..Default::default()` needed, no S3 scope creep.

### P3-3: Command registration

**File:** `crates/zeph-core/src/agent/mod.rs` (~L592-661)

Import + `agent_reg.register(ThinkTokensCommand)` / `register(ReasoningEffortCommand)` —
channel-agnostic, serves CLI/TUI/Telegram/Discord/Slack with one registration.

### P3-4: Unit/integration tests (Phase 3)

- `handle_think_tokens`/`handle_reasoning_effort` no-arg display, valid set, invalid parse,
  unsupported-provider paths
- S2: switch after active override shows notice; switch with no override shows no notice

---

## Phase 4: Binary (`src/`)

### P4-1: CLI flag

**File:** `src/cli.rs`

```rust
#[arg(long, value_name = "LEVEL", value_parser = ["low", "medium", "high"])]
reasoning_effort: Option<String>,
```

No `--think-tokens` flag (M2).

### P4-2: Startup application

**File:** `src/runner.rs`

Apply `cli.reasoning_effort` at startup, next to the existing `--thinking` application block
(~L1282-1289). Fan out to applicable provider entries: Claude → `Adaptive` effort,
OpenAI/Compatible → `reasoning_effort`, Gemini → `thinking_level`. Startup token budget for
Gemini/OpenAI remains config-only (M2) — do not add fan-out for token budget here.

### P4-3: `--init` wizard prompt

**File:** `src/init/mod.rs`

Prompt for a default reasoning effort when configuring a Claude/OpenAI/Gemini provider
(thinking-budget default already exists via the provider's `thinking`/`thinking_budget` config
fields — no new prompt needed for that).

---

## Mandatory Integration Points (CLAUDE.md, all 7 covered)

| # | Point | Where |
|---|-------|-------|
| 1 | `config.toml` section | N/A — no new field; startup defaults already exist on `ProviderEntry`; document live-override + S2 reset behavior in `docs/src/` |
| 2 | CLI flag | P4-1, `--reasoning-effort` |
| 3 | TUI command palette | P2-4, `CommandInfo` entries + channel-agnostic `agent_reg` registration (P3-3); no spinner needed (instant local mutation) |
| 4 | `--init` wizard | P4-3 |
| 5 | `--migrate-config` | N/A, trivially — no config.toml parameter, no SQLite change (S3); record the rationale in the PR description |
| 6 | Testing playbook | Create `/Users/rabax/Dev/zeph/.local/testing/playbooks/thinking-reasoning-runtime.md` (main-repo path) |
| 7 | Coverage status | Add rows in `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` for `/think-tokens` and `/reasoning-effort` (status `Untested`) |

---

## spec-003 Clarification (applied alongside code)

Add a one-line clarification to `specs/003-llm-providers/spec.md` §"Key Invariants": the
`&self` invariant scopes to request/inference methods only; in-place config setters applied
between turns (this feature's `set_thinking`/`set_thinking_budget`/`set_thinking_level`/
`apply_reasoning_effort`, and the pre-existing `set_reasoning_effort`) are exempt. Applied as
part of this spec package (see `[[specs/070-runtime-thinking-controls/spec]]` §7 addendum).

---

## Pre-Merge Checklist

- [ ] `cargo +nightly fmt --check`
- [ ] `cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`
- [ ] `cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler" --lib --bins`
- [ ] `RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] `cargo test --doc --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] `./.github/scripts/add-spdx-headers.sh` run on the two new handler files
- [ ] `CHANGELOG.md` updated (`[Unreleased]`)
- [ ] `.local/testing/playbooks/thinking-reasoning-runtime.md` created (main-repo path)
- [ ] `.local/testing/coverage-status.md` rows added (main-repo path)
- [ ] `specs/003-llm-providers/spec.md` clarification line present
- [ ] LLM serialization gate: live session test on Claude touching `claude/mod.rs` thinking
      path — verify debug-dump payload shows correct `max_tokens`/`thinking` across an
      enable→off→re-enable cycle (per `.claude/rules/continuous-improvement.md` LLM
      Serialization Gate — `claude.rs`/`any.rs` are in the gated file list)
