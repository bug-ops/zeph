---
aliases:
  - Runtime Thinking Controls Tasks
  - Think Tokens Reasoning Effort Tasks
  - Tasks 3098
tags:
  - sdd
  - tasks
  - llm
  - core
created: 2026-07-10
status: approved
related:
  - "[[specs/070-runtime-thinking-controls/plan]]"
  - "[[specs/070-runtime-thinking-controls/spec]]"
---

# Task Breakdown: Runtime Thinking Controls — `/think-tokens` and `/reasoning-effort` (GitHub #3098)

All tasks reference `[[specs/070-runtime-thinking-controls/plan]]`. This is the developer's
primary implementation checklist alongside the architect handoff
(`.local/handoff/2026-07-10T15-50-16-architect.md`). Implement in phase order — each phase
depends only on prior phases.

---

## Phase 1: `zeph-llm` Provider Layer

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T1.1 | Add `base_max_tokens: u32` field + constructor capture | P1-1 | `crates/zeph-llm/src/claude/mod.rs` | Captured before any `with_thinking` chain (S1) |
| T1.2 | Add `set_thinking(&mut self, Option<ThinkingConfig>) -> Result<(), LlmError>` | P1-1 | `crates/zeph-llm/src/claude/mod.rs` | `None` restores baseline; `Some(_)` recomputes from baseline, not current `max_tokens` |
| T1.3 | Refactor `with_thinking` to delegate to `set_thinking` | P1-1 | `crates/zeph-llm/src/claude/mod.rs` | DRY; construction-time behavior unchanged |
| T1.4 | Add `current_thinking_budget`/`current_reasoning_effort` getters | P1-1 | `crates/zeph-llm/src/claude/mod.rs` | Display path + S2 input |
| T1.5 | Add `set_thinking_budget(&mut self, Option<i32>)` + `set_thinking_level(&mut self, Option<GeminiThinkingLevel>)` + getters | P1-2 | `crates/zeph-llm/src/gemini/mod.rs` | Mirrors existing consuming builders |
| T1.6 | Add `ReasoningEffort` enum (`Low\|Medium\|High`, `as_str`/`FromStr`) | P1-3 | `crates/zeph-llm/src/any.rs` | Owned by `zeph-llm` |
| T1.7 | Add `set_thinking_budget(&mut self, Option<u32>) -> Result<(), LlmError>` fan-out | P1-3 | `crates/zeph-llm/src/any.rs` | `None` disable mapped per-provider (M1); cover `Masked(inner)` |
| T1.8 | Add `apply_reasoning_effort(&mut self, ReasoningEffort) -> Result<(), LlmError>` fan-out | P1-3 | `crates/zeph-llm/src/any.rs` | Claude/OpenAI/Compatible/Gemini branches; others NotSupported |
| T1.9 | Add `current_thinking_budget`/`current_reasoning_effort` fan-out getters | P1-3 | `crates/zeph-llm/src/any.rs` | Cover `Masked(inner)` |
| T1.10 | Unit tests: Claude S1 idempotency (enable→off→enable), out-of-range rejection, construction-parity edge case | P1-4 | `crates/zeph-llm/src/claude/mod.rs` | S1 regression coverage — required, not optional |
| T1.11 | Unit tests: Gemini `off`→`Some(0)` mapping, out-of-range rejection | P1-4 | `crates/zeph-llm/src/gemini/mod.rs` | M1 coverage |
| T1.12 | Unit tests: `AnyProvider` fan-out per variant incl. `Masked`, NotSupported paths | P1-4 | `crates/zeph-llm/src/any.rs` | |

**Phase 1 gate:** `cargo nextest run -p zeph-llm` green before starting Phase 2.

---

## Phase 2: `zeph-commands` Command Layer

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T2.1 | New `ThinkTokensCommand` `CommandHandler` struct | P2-1 | `crates/zeph-commands/src/handlers/think_tokens.rs` | New file — run `add-spdx-headers.sh` |
| T2.2 | `parse_token_budget(&str) -> Result<Option<u32>, String>` + edge-case unit tests | P2-1 | `crates/zeph-commands/src/handlers/think_tokens.rs` (or `mod.rs`) | Test: empty, `k`, `-1`, `1.2.3k`, `off`, `0`, `8k`, `10.5k`, `1M`, overflow |
| T2.3 | New `ReasoningEffortCommand` `CommandHandler` struct | P2-1 | `crates/zeph-commands/src/handlers/reasoning_effort.rs` | New file — run `add-spdx-headers.sh` |
| T2.4 | `pub mod` registration | P2-2 | `crates/zeph-commands/src/handlers/mod.rs` | |
| T2.5 | Two `AgentAccess` trait methods + two `NullAgent` stubs | P2-3 | `crates/zeph-commands/src/traits/agent.rs` | ~L172-189, ~L671-690 |
| T2.6 | Two `CommandInfo` entries in Configuration block | P2-4 | `crates/zeph-commands/src/commands.rs` | Drives `/help`; TUI autocomplete uses a separate registry (pre-existing gap, not fixed here) |

**Phase 2 gate:** `cargo nextest run -p zeph-commands` green before starting Phase 3.

---

## Phase 3: `zeph-core` Agent Layer

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T3.1 | Implement `handle_think_tokens` on `Agent<C>` | P3-1 | `crates/zeph-core/src/agent/agent_access_impl.rs` | Parse → capability check → setter → format; template `handle_caveman` |
| T3.2 | Implement `handle_reasoning_effort` on `Agent<C>` | P3-1 | `crates/zeph-core/src/agent/agent_access_impl.rs` | Same pattern |
| T3.3 | Capture `had_reasoning_override` before `set_provider` | P3-2 | `crates/zeph-core/src/agent/provider_cmd.rs` | Read old provider instance's getters before replacement |
| T3.4 | Extend `build_switch_message` signature + reset notice | P3-2 | `crates/zeph-core/src/agent/provider_cmd.rs` | Both branches; notice only when `had_reasoning_override` |
| T3.5 | Verify `ProviderOverrides {...}` literal (:488-490) untouched | P3-2 | `crates/zeph-core/src/agent/provider_cmd.rs` | S3 boundary check — no `..Default::default()` added |
| T3.6 | Register both commands on `agent_reg` | P3-3 | `crates/zeph-core/src/agent/mod.rs` | ~L592-661; channel-agnostic |
| T3.7 | Unit tests: handler no-arg display, valid set, invalid parse, unsupported-provider | P3-4 | `crates/zeph-core/src/agent/agent_access_impl.rs` | |
| T3.8 | Unit tests: S2 notice fires only when override was active | P3-4 | `crates/zeph-core/src/agent/provider_cmd.rs` | Both branches (present/absent) |

**Phase 3 gate:** `cargo nextest run -p zeph-core` green before starting Phase 4.

---

## Phase 4: Binary (`src/`)

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T4.1 | Add `--reasoning-effort <low\|medium\|high>` CLI arg | P4-1 | `src/cli.rs` | `value_parser = ["low","medium","high"]`; no `--think-tokens` (M2) |
| T4.2 | Apply `cli.reasoning_effort` at startup, fan out per provider | P4-2 | `src/runner.rs` | Next to existing `--thinking` block (~L1282-1289) |
| T4.3 | `--init` wizard reasoning-effort default prompt | P4-3 | `src/init/mod.rs` | Claude/OpenAI/Gemini branches |

**Phase 4 gate:** full workspace build + `cargo run -- --reasoning-effort high` smoke test.

---

## Phase 5: Documentation and Cross-Cutting (CLAUDE.md Mandatory Integration Points)

| # | Task | Integration Point | Path | Notes |
|---|------|--------------------|------|-------|
| T5.1 | Document live-override + S2 reset behavior | #1 config.toml | `docs/src/` | No new schema field; document runtime-only semantics |
| T5.2 | Create testing playbook | #6 | `/Users/rabax/Dev/zeph/.local/testing/playbooks/thinking-reasoning-runtime.md` | Main-repo path (not worktree); scenarios per architect handoff §"Testing playbook" |
| T5.3 | Add coverage-status rows | #7 | `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` | Two new rows, status `Untested`, main-repo path |
| T5.4 | Apply spec-003 `&self` invariant clarification | — | `specs/003-llm-providers/spec.md` | Applied by `sdd` agent as part of this spec package (already done — verify present) |
| T5.5 | Update `CHANGELOG.md` `[Unreleased]` | — | `CHANGELOG.md` | Root |
| T5.6 | Live LLM serialization gate test | — | — | Claude session, enable→off→re-enable cycle; inspect debug-dump `max_tokens`/`thinking` payload (gate applies — `claude/mod.rs`/`any.rs` touched) |

**#2 (CLI flag) covered by T4.1. #3 (TUI palette) covered by T2.6 + T3.6 — no additional task,
no spinner needed (instant local mutation). #4 (`--init` wizard) covered by T4.3. #5
(`--migrate-config`) is N/A — no task, record the rationale in the PR description.**

---

## Acceptance Criteria (for PR merge)

- [ ] All Phase 1-4 unit tests pass: `cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler" --lib --bins`
- [ ] `cargo +nightly fmt --check`
- [ ] `cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`
- [ ] Rustdoc gate: `RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] Doc-tests: `cargo test --doc --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] S1 regression test present and passing (Claude enable→off→re-enable max_tokens restore)
- [ ] S2 notice test present and passing (both fire/no-fire branches)
- [ ] S3 verified: no diff in `crates/zeph-config/src/providers/entry.rs`, no new
      `ProviderOverrides` field
- [ ] `add-spdx-headers.sh` run on `think_tokens.rs` and `reasoning_effort.rs`
- [ ] `CHANGELOG.md` updated
- [ ] Testing playbook + coverage-status rows added (main-repo `.local/testing/` path)
- [ ] Live LLM serialization gate test documented in PR description (Claude thinking
      enable/disable round-trip, no 400/422, well-formed debug-dump payload)
- [ ] `specs/003-llm-providers/spec.md` clarification line present
- [ ] `specs/README.md` registers `070-runtime-thinking-controls`
