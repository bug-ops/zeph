---
aliases:
  - Runtime Thinking Controls BRD
  - Think Tokens Reasoning Effort BRD
  - BRD 3098
tags:
  - sdd
  - brd
  - llm
  - core
created: 2026-07-10
status: approved
related:
  - "[[specs/070-runtime-thinking-controls/spec]]"
  - "[[specs/070-runtime-thinking-controls/srs]]"
  - "[[specs/070-runtime-thinking-controls/nfr]]"
  - "[[specs/003-llm-providers/spec]]"
  - "[[specs/042-zeph-commands/spec]]"
---

# BRD: Runtime Thinking Controls — `/think-tokens` and `/reasoning-effort` (GitHub #3098)

## 1. Business Context

Aider (Python AI pair-programming CLI, a tracked competitive-parity reference agent per
`.claude/rules/continuous-improvement.md`) supports runtime slash commands that adjust a
model's reasoning depth mid-session, without restarting the process. Zeph currently exposes
equivalent knobs (`thinking` for Claude, `reasoning_effort` for OpenAI/Compatible,
`thinking_budget`/`thinking_level` for Gemini) only as static `config.toml` fields or a
one-shot `--thinking` CLI flag applied at startup. Changing them mid-session requires exiting
and restarting the agent.

## 2. Problem Statement

A user tuning thinking depth/cost during an active session — e.g. dropping to a cheap,
low-effort mode for a trivial follow-up question, or raising the token budget for a hard
debugging turn — must currently kill the session, edit `config.toml` or re-pass `--thinking`,
and restart. This breaks conversational flow and discards in-memory context. Aider users get
this control without leaving the session.

## 3. Business Goals

| ID | Goal | Priority |
|----|------|----------|
| BG-01 | Users can change reasoning token budget (`/think-tokens`) and reasoning effort level (`/reasoning-effort`) mid-session via slash commands, with no restart | P1 |
| BG-02 | The change takes effect on the very next turn, with no perceptible delay (pure in-memory mutation, no I/O, no spinner needed) | P1 |
| BG-03 | Providers or parameters that don't support a knob give a clear, actionable message — never a silent no-op | P2 |
| BG-04 | Switching the active provider (`/provider`) after a runtime override does not silently discard that override without telling the user | P2 |
| BG-05 | The controls are available uniformly across CLI, TUI, and channel adapters (Telegram/Discord/Slack) via the existing channel-agnostic command registry | P2 |

## 4. Stakeholders

| Role | Interest |
|------|----------|
| CLI/TUI power user | Tune cost vs. quality mid-session without losing conversational context |
| Channel user (Telegram/Discord/Slack) | Same runtime control parity as CLI/TUI — channel-agnostic registration |
| Zeph maintainers | Close the Aider competitive-parity gap tracked under `.local/testing/playbooks/competitive-parity.md` |

## 5. Out of Scope

| Item | Reason |
|------|--------|
| Persisting overrides across process restart / `/provider` switch (SQLite, `ProviderOverrides`) | Issue #3098 explicitly specifies "session only, not persisted" (S3) — see `[[specs/070-runtime-thinking-controls/spec]]` §S3 |
| A startup `--think-tokens` CLI flag (token-budget analog of `--reasoning-effort`) | `--thinking extended:N` already covers Claude-only startup token budget (M2); no clean cross-provider startup semantics exists for token budget, and duplicating it would be premature sugar |
| OpenAI `minimal` reasoning-effort value (newer gpt-5.x models) | MVP keeps the existing 3-value contract (`low\|medium\|high`) of `OpenAiProvider::set_reasoning_effort` (M3); flagged as a follow-up issue candidate |
| A dirty-flag refinement to avoid the S2 "had override" over-warn on config-default thinking | Non-blocking per critic review; conservative over-warning is factually accurate, not a correctness bug |

These deferrals are explicit, not silent omissions, and are carried into `srs.md` as
acknowledged-deferred requirements.

## 6. Success Criteria

| ID | Criterion | Measurable |
|----|-----------|-----------|
| SC-01 | `/think-tokens 8k` on a Claude session changes the effective thinking budget sent on the next turn | Debug-dump payload inspection shows updated `budget_tokens` |
| SC-02 | `/think-tokens off` after `/think-tokens 8k` restores `max_tokens` to its original config value, not the 16k floor | S1 regression test: enable → off → re-enable cycle, inspect `max_tokens` at each step |
| SC-03 | `/reasoning-effort high` on an OpenAI/Compatible session updates the `reasoning_effort` field sent on the next turn | Debug-dump payload inspection |
| SC-04 | Invoking either command with no argument displays the current setting | Manual/CLI test: no-arg invocation returns a formatted status string |
| SC-05 | Invoking either command against an unsupported provider (e.g. Ollama) returns a clear "not supported by provider X" message, never a silent no-op | Manual test across Claude/OpenAI/Gemini/Ollama |
| SC-06 | `/provider` switch after a runtime override was active on the old provider prints a reset-notice; switching with no prior override does not | Manual test both branches |
| SC-07 | `--reasoning-effort <low\|medium\|high>` at startup applies to the configured provider before the first turn | CLI integration test |
| SC-08 | Commands are usable identically from CLI, TUI, and any configured channel adapter | Cross-mode consistency test per `.claude/rules/continuous-improvement.md` |

## 7. Constraints

- Session-only / non-persistent: no SQLite schema change, no `ProviderOverrides` field addition (S3)
- No new lock or blocking synchronization primitive is introduced — mutation happens in-place
  (`&mut self`) at the turn boundary, consistent with the existing `/provider` switch and
  `set_reasoning_effort` restore-path precedent
- Zero new `[[llm.providers]]` config fields — startup defaults already exist on `ProviderEntry`
- MSRV and workspace feature-flag structure unchanged
- Implementation stays within `zeph-llm`, `zeph-commands`, `zeph-core`, and the root binary
  crate (`src/`); `zeph-config` is not touched

## 8. Dependencies

| Dependency | Type | Notes |
|------------|------|-------|
| `AnyProvider` enum fan-out (`crates/zeph-llm/src/any.rs`) | Internal | Existing dispatch pattern extended with new setters/getters |
| `provider: AnyProvider` field on `Agent<C>` (`crates/zeph-core/src/agent/mod.rs:161`) | Internal | Not `Arc<dyn>`, not lock-wrapped — mutation site for both commands |
| Existing `set_reasoning_effort(&mut self, Option<String>)` (OpenAI-only restore path) | Internal | Left unchanged; new `apply_reasoning_effort` fan-out is a separate, parallel method (S3 boundary) |
| `CommandHandler` / `AgentAccess` trait infrastructure (`zeph-commands`) | Internal | Template: `handlers/model.rs` and `handle_caveman` |
| `handle_provider_switch` (`crates/zeph-core/src/agent/provider_cmd.rs:457-497`) | Internal | Extension point for the S2 reset-notice |
| `--thinking extended:N` CLI flag (`src/runner.rs:1282`) | Internal | Existing Claude-only startup precedent; `--reasoning-effort` is added alongside it |
