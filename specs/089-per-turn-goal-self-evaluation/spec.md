---
aliases:
  - Per-Turn Goal Self-Evaluation
  - /goal check
tags:
  - sdd
  - spec
  - agent-loop
  - goal-supervision
created: 2026-11-10
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[002-agent-loop/spec]]"
  - "[[039-background-task-supervisor/spec]]"
---

# Spec: Per-Turn Goal Self-Evaluation (`/goal check`)

> [!info]
> One advisory `GoalSupervisor::verify` call, runnable from any ordinary chat session, that judges a goal condition against the recent conversation. Promoted from the research draft 058 (Goose `/goal` parity gap). GitHub #6009. The opt-in turn-end auto-trigger of the draft is deferred (see section 9).

## 1. Problem

An interactive session has no way to ask "did you actually do X?" short of starting a full autonomous goal session (`/goal create --auto`), whose turn budget, stuck detection and lifecycle state are built for unattended runs. The judgment primitive already exists (`GoalSupervisor::verify`, structured JSON: `achieved`, `reasoning`, `confidence`, `suggestions`) but is only reachable from `autonomous_turn.rs`.

## 2. Command Surface

`/goal check [condition]`, a subcommand of the existing `/goal` command (no new command name, trait method or registry entry). `check: <condition>` is accepted too.

Condition resolution, first match wins, and the output always prints the condition and its source:

| Source | When |
|--------|------|
| `explicit` | text after `check` |
| `active goal` | no text, `[goals]` enabled, goal store available and a goal is `Active` |
| `last user message` | no text and no active goal: the last agent-visible `Role::User` message that has a `Text` part and no `ToolResult` part (text parts only, truncated to 500 chars, a leading `/goal` message is skipped) |

With no source the reply asks the user to state a condition and no LLM call is made.

## 3. Requirements

| ID | Requirement |
|----|-------------|
| FR-001 | THE SYSTEM SHALL run exactly one `GoalSupervisor::verify` call per `/goal check` and print `achieved`, `reasoning`, `confidence` and, when not achieved and non-empty, `suggestions`. |
| FR-002 | THE SYSTEM SHALL NOT create, read or mutate any `AutonomousSession` / `AutonomousState`. |
| FR-003 | WHEN an autonomous goal session is running THE SYSTEM SHALL reject the command and point to `/goal status`, so two verifiers never race one session. |
| FR-004 | THE SYSTEM SHALL resolve the provider and timeout from `[goals] supervisor_provider` / `supervisor_timeout_secs` (empty falls back to the main provider; an unresolvable name logs at debug level and falls back). No new config key. |
| FR-005 | THE SYSTEM SHALL work when `[goals] enabled = false`: `check` is dispatched before the goals-enabled gate because it needs no database; only the active-goal source is skipped. |
| FR-006 | WHEN the call fails (LLM error, timeout, 429, parse failure) THE SYSTEM SHALL return a non-fatal message; the timeout message shows the configured value. |
| FR-007 | THE SYSTEM SHALL scrub secrets (`scrub_content`) from the conversation summary and from tool arguments before they reach the supervisor provider. `recent_actions` are the last 10 `ToolUse` parts as `name(args)`, args truncated to 100 chars after scrubbing. |
| FR-008 | THE SYSTEM SHALL show a `Verifying goal…` status while the call runs (spinner in the TUI) and clear it afterwards. |

## 4. Non-Functional Requirements

- The verdict is advisory only: it never gates, blocks or retries tool execution or turn completion. `GoalSupervisor` stays "a heuristic verifier, not a security boundary".
- Observability: span `core.goal.turn_selfcheck` (field `source`) around the verify call; the existing `goal.supervisor.verify` span nests inside.
- Cost: at most two LLM calls (initial plus one parse retry inside `chat_json`), only on explicit user request.
- Cross-mode: the handler lives in `zeph-core`/`zeph-commands`, so syntax and output are identical in CLI, TUI and every other slash-command channel.

## 5. Design

- `crates/zeph-core/src/agent/goal_selfcheck.rs`: `GoalConditionSource`, condition resolution, a `GoalCheckJob` holding only owned, `Send` data, and `Agent::goal_check`.
- `handle_goal` dispatches `check` first. The `/goal` goals-accounting initialisation moved into `Agent::ensure_goal_accounting`, shared by both paths.
- `build_supervisor` and `build_conversation_summary` in `autonomous_turn.rs` became `pub(super)`; autonomous behavior is unchanged.
- The command is user-initiated and awaited, the same precedent as `/skill create`. No `tokio::spawn`; the only status channel used is the existing `status_tx`.

## 6. Edge Cases

| Scenario | Behavior |
|----------|----------|
| Empty history, no condition | ask the user to state one, no LLM call |
| Last user message is a tool-result carrier | skipped; the preceding real request is used |
| Provider name in `supervisor_provider` not found | main provider, debug log |
| HTTP 429 | "temporarily rate-limited" message, no retry loop |

## 7. Success Criteria

- A test proves `check` leaves `AutonomousDriver::session` untouched and is rejected while a session is running.
- A test proves the command works with `[goals] enabled = false`.
- Condition-source resolution, tool-result skipping, secret scrubbing of actions and report formatting are unit-tested.

## 8. Agent Boundaries

**Always**: reuse `GoalSupervisor`/`SupervisorVerdict`; keep the call inside a `tracing::info_span!`.

**Never**: use the verdict to gate or retry tool execution or turn completion; add an unsupervised `tokio::spawn`; change the autonomous periodic supervisor check.

## 9. Deferred

- Opt-in turn-end auto self-check (draft US-003/FR-005): needs a concrete "turn concludes a task" heuristic and a config flag; deferred to a follow-up.
- Re-confirming the parity gap against Cline/RooCode, OpenHands and Aider.

## 10. See Also

- `crates/zeph-core/src/goal/supervisor.rs`, `crates/zeph-core/src/agent/autonomous_turn.rs`
- `crates/zeph-commands/src/handlers/goal.rs`
