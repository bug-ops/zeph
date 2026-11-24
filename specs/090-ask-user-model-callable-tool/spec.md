---
aliases:
  - ask_user tool
  - Model-callable question tool
tags:
  - sdd
  - spec
  - agent-loop
  - tools
  - channels
created: 2026-11-10
status: approved
related:
  - "[[constitution]]"
  - "[[MOC-specs]]"
  - "[[006-tools/spec]]"
  - "[[007-channels/spec]]"
  - "[[013-acp/spec]]"
  - "[[039-background-task-supervisor/spec]]"
---

# Spec: Model-callable `ask_user` tool in the main agent loop

> [!info]
> The main-loop model can pause a turn with one `ask_user` call, ask the user a structured question (free text or a choice list) and receive the answer, or an explicit decline, cancel or timeout, as the tool result, with the same semantics on every channel. Opt-in. GitHub #6912.

## 1. Problem

When the model lacks a fact only the user has (a choice between valid options, a missing parameter, a scope decision) it must guess or end the turn with prose that breaks tool-loop continuity. Zed, ZeroClaw, OpenCode and Claude Code (AskUserQuestion) let the model ask. Zeph had MCP elicitation and DAG-only human-in-the-loop but no model-callable ask in the main loop.

## 2. Out of Scope

- MCP server-initiated elicitation (model-initiated here) and DAG human-in-the-loop gates (#6234).
- Forms beyond one question with free text or single/multi choice.
- Persisting answers as memory or auto-learned preferences.
- Subagents: offered only through an explicit grant (`permissions.ask_user: allow`), foreground `/agent spawn` and `/agent resume` only; background and DAG sub-agents never get the tool.

## 3. Functional Requirements

| ID | Requirement |
|----|-------------|
| FR-001 | An `ask_user` call suspends that tool call, shows the question on the active channel and returns the answer as the tool result. |
| FR-002 | Question kinds: free text, single choice, multi choice (2 to 8 distinct options), each choice optionally accepting a free-text "other". |
| FR-003 | The capability is two defaulted methods on the `Channel` trait: `ask_user`, whose default declines (`Declined(NonInteractive)`), and `ask_user_support() -> AskUserSupport { Interactive, Unavailable }`, whose default is `Unavailable`. |
| FR-004 | A non-interactive channel (piped stdin, no responder, Telegram guest context) returns `Declined` at once without blocking. |
| FR-005 | The user cancelling (`cancel`, `/cancel`, a slash command, Esc, Ctrl-C/Ctrl-D, dismissed dialog, turn cancel) returns `Cancelled`. |
| FR-006 | Implemented on CLI (TTY), TUI, Telegram, Discord, Slack, ACP and `zeph serve` (`ask_user` SSE event, `POST /sessions/{id}/answer`). |
| FR-007 | Off by default: `[tools.ask_user] enabled = false` removes the tool from the tool list. `--ask-user` enables it for a session. |
| FR-008 | `timeout_secs` (default 300, at most 3600) bounds the wait; expiry returns `TimedOut`. The agent owns the deadline; channels apply none. |
| FR-009 | `max_questions_per_turn` (default 3) caps questions; question and option text are sanitized and bounded. |
| FR-010 | `[tools.ask_user]` config, `--migrate-config` step, `--init` wizard step, CLI flag, playbook and coverage row. |
| FR-011 | The tool is offered to the model, and a call is answered, only when `ask_user_offered()` holds: enabled, the turn origin is `User` and `ask_user_support()` is `Interactive`. Otherwise it is absent from the tool list and a call returns `Declined(NonInteractive)` with no channel call and no question slot used (#7122). |
| FR-012 | A `[tools.permissions]` deny rule on `ask_user` removes it from the tool list (all rules `Deny`) and blocks a matching call before the channel is asked (spec 006, #7123). |

## 4. Non-Functional Requirements

- NFR-001 Responsiveness: the wait never blocks input, render or adapters; no lock is held across the await; the channel future is cancel-safe.
- NFR-002 Observability: status line `Waiting for your answer...`, span `core.tools.ask_user` (fields `outcome`, `answer_len`).
- NFR-003 Security: see section 6.
- NFR-004 Type safety: kinds, outcomes and replies are enums (`AskUserOutcome`, `AskUserAnswer`, `ReplyKind`), questions and options are validated newtypes.

## 5. Design

**Types** (`zeph-core::channel`): `AskUserRequest` (private `QuestionText`, `AskUserKind`), `ChoiceOptions`, `OtherAnswer`, `AskUserAnswer::{Text, Selected{indices}}`, `AskUserOutcome::{Answered, Declined(DeclineReason), Cancelled, TimedOut}`, `ReplyKind::{Cancel, Command, Answer}` with `classify_reply`, `parse_reply`, the form helpers `dialog_message`, `closed_choice_values`, `dialog_answer_hint`, `parse_dialog_answer`, and the constant `ASK_USER_HEADER`.

**Support and origin** (#7122): overrides return `Interactive` for CLI (stdin is a TTY), TUI, Discord, Slack, Telegram (`Unavailable` while `guest_query_id` is set), Loopback/ACP (a responder is attached) and forward through `AnyChannel` and the app channel; the gateway wrapper returns `Unavailable` after a webhook `recv`. `AskUserState::begin_turn(origin)` records the `TurnOrigin`; scheduler, `/loop`, bot-sent and autonomous goal turns (`TurnOrigin::Injected`) never offer the tool.

**Tool**: a built-in answered in the agent loop (`agent/ask_user.rs`), listed only while offered (FR-011). Calls are serialized; calls refused by the pre-exec, quota or repeat gates get a failed reply and never reach the user. One choke point applies the per-turn cap, `timeout(cfg.timeout, channel.ask_user(req))` raced with the turn cancel token, and maps `ChannelClosed` to `Cancelled`. Tool input: `question` (required), `options` (2-8), `multi_select`, `allow_other`.

**Reply handling** (`classify_reply`): `cancel` or `/cancel` (trimmed, case-insensitive) cancels; text selecting a registered slash command (`CommandRegistry::find_handler` rule) cancels with the notice "Question cancelled - resend your command"; anything else, including `/home/me/proj`, is an answer. Bot prompts list `cancel` as an instruction, never as a numbered option.

**Parsing** (`parse_reply`): a position (1-based, comma-separated for multi), an exact label (case-insensitive), or free text when other is allowed. The returned indices are non-empty, ascending and below the option count.

**Channels**: bot adapters share one flow over `ConfirmLoop::run_reply` (no own deadline): header first, numbered options, an invalid reply re-prompts. Replies must come from the chat or channel of the question and never from a bot (Telegram `is_from_bot`). CLI TTY holds the stdin coordinator and reads with the yieldable line editor; the coordinator is released only when the blocking read has returned. TUI reuses the elicitation dialog (a closed single choice is an enum field of `"{n}. {label}"` values; everything else a text field); Esc returns `Cancelled` only; the dialog closes on the next tick when its requester dropped the answer channel. ACP shows the question through the session's elicitation bridge (`AcpAskUserResponder`, attached with `LoopbackChannel::with_ask_user_responder`); without the IDE capability it declines. The bridge applies no timeout (`[acp.timeouts].elicitation_secs` does not bound `ask_user`); when the agent deadline drops the question the bridge is freed for the next request, but the form already shown stays in the IDE until the user dismisses it (the protocol has no withdraw call).

**Sub-agents**: a granted foreground sub-agent's call is forwarded to the parent over a per-agent channel and answered by the parent's `ask_user` choke point (cap shared within the current command's ask scope, deadline raced with the sub-agent cancel token, `redact_secrets`, channel sanitizing). Each slash command is its own ask scope (fresh counter, the input's origin; an Injected origin never offers the tool). The FR-012 deny rule is checked per call; a wholesale deny rule means the offer is not made. The sub-agent tool policy (allow list, `tools.except`) still applies. The sub-agent's `timeout_secs` keeps running while a question is open. Known limit: a foreground `/agent` right after a cancelled turn starts with an already-cancelled token, so its questions return `Cancelled`.

**Serve**: `zeph serve` is an authenticated operator channel, so answers get no guardrail pre-screen; free-text answers are sanitized as a `ChannelMessage`, and the choke point applies `redact_secrets`. With no SSE subscriber at ask time the question is declined at once.

**TUI multi-choice**: a closed multi-choice is a checklist (Space marks, Enter submits); submitting with nothing marked re-asks with the retry hint, because the dialog closes on every submit.

## 6. Security

- The fixed header ("Question from the assistant - never share passwords, API keys or other secrets") is a constant rendered first by every channel; the model cannot alter it.
- Question and option text: control, bidirectional and zero-width characters are stripped, blank runs collapse, at most 6 lines, options on one line; each adapter neutralizes its markup (Discord/Slack mentions, Slack `<url|text>`, Discord/Telegram markdown links).
- Every answer is scrubbed with `redact_secrets` at the choke point, before it enters history, memory, the debug dump, the audit log or the model, then takes the incoming-channel-message path (`sanitize_channel_text_if_untrusted`), not the tool-output sanitizer. The raw answer is never logged. `redact_secrets` is pattern-based: the header is the primary control, the scrubber is defense in depth.
- `ClaimSource::UserAnswer` maps to native provenance and the `LocalUntrusted` tier. Its only producers are the main handler and the crate-private sub-agent relay; the sub-agent loop sanitizes every output.
- A label made only of digits is rejected, so a numeric reply is always a position.

## 7. Key Invariants

1. Disabled by default; the tool is absent from the tool list while disabled, on a channel that cannot answer, in an injected-origin turn, and in a sub-agent without the explicit grant.
2. Every outcome, including cancel and timeout, leaves a paired tool result and the turn continues.
3. A channel applies no deadline; dropping its future releases stdin, the dialog and receivers.
4. Answers are scrubbed before any persistence or model exposure.
5. `InteractionConfig` has no `Default`: every build site (CLI, daemon, ACP, serve) must set it.
6. A reply of one session never answers another channel's question.

## 8. NEVER

- Spawn an unsupervised task for a question; hold a lock across the wait.
- Auto-answer on the user's behalf or log the answer text.
- Offer the tool to a subagent without an explicit grant.
- Let model text replace the fixed header.

## 9. Acceptance Criteria

- [ ] SC-001 round trip on CLI, TUI, bot channels and ACP (live checks per `playbooks/ask-user.md`); a real model-driven round trip is verified on 9df5f71b (live run G1), the remaining channels are not yet run live.
- [x] SC-002 piped CLI returns `Declined` in under 100 ms.
- [x] SC-003 tool-call/result pairing stays valid for all four outcomes.
- [x] SC-004 disabled config removes the tool; without a grant, or for background/DAG sub-agents, the tool is absent.

## 10. Config and Operations

`[tools.ask_user]` `enabled` (false), `timeout_secs` (300, validated to `1..=3600` by `AskUserTimeout`; a value outside the range fails config loading), `max_questions_per_turn` (3, non-zero). Startup-only: not hot-reloaded. Migration step 118, wizard step, `--ask-user`.

## 11. See Also

- [[006-tools/spec]], [[007-channels/spec]], [[013-acp/spec]], [[039-background-task-supervisor/spec]]
