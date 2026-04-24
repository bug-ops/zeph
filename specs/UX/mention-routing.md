---
aliases:
  - At-Agent Mention Routing
  - "@agent Mention Spec"
tags:
  - sdd
  - ux
  - tui
  - a2a
  - research
created: 2026-04-24
status: research
related:
  - "[[028-hooks/spec.md]]"
  - "[[014-a2a/spec.md]]"
  - "[[013-acp/spec.md]]"
  - "[[011-tui/spec.md]]"
---

# @agent Mention Routing — Research Spec (#3327)

## Purpose

This document assesses the feasibility of routing a user message to a specific
sub-agent or orchestrated agent when the message begins with an `@name` mention
(e.g. `@coder fix the build`, `@researcher summarize this paper`). It documents
the Goose reference implementation, identifies what Zeph would need to change,
and delivers a deferred/implement verdict with rationale.

---

## 1. Reference: Goose @agent Pattern

[Goose](https://github.com/block/goose) (Block) supports `@<extension>` targeting
in its REPL and TUI. When a user types `@shell ls -la`, Goose routes the message
directly to the `shell` extension, bypassing the orchestrator LLM. The routing is
applied at the REPL input boundary before the message enters the turn loop.

Key properties of the Goose approach:

- **Prefix match** — the `@name` prefix is stripped before forwarding; the
  extension receives only the remainder of the message.
- **Exact name match** — extension names are registered at startup; unknown
  mentions fall through to the default agent (no error).
- **No LLM involvement** — routing is deterministic and zero-cost.
- **No session state change** — the main conversation history receives the full
  original message; the extension call is a side-effect dispatch.

---

## 2. Zeph's Current Architecture

### Input parsing

User input enters through `Channel::recv` (CLI reads from stdin; TUI reads from
the Insert-mode input box; Telegram receives the update body). All three paths
converge in the agent loop's `process_user_message` before any slash-command
dispatch.

Slash commands are dispatched in `dispatch_slash_command` after a `/` prefix
check. An `@name` mention would require an analogous prefix check at the same
site.

### Agent identity

Zeph does not currently have a runtime registry of named agents. Sub-agents are
spawned ad hoc by `/agent spawn` or `[[acp.subagents.presets]]`. There is no
static name-to-endpoint map available at input-parse time.

### A2A dispatch

`zeph-a2a` supports invoking a named remote agent via `A2AClient::send_task`. A
mention like `@remote-analyst` could resolve to an A2A endpoint if a name map
were maintained in `AgentCard` discovery. However, A2A calls are async and
require an established connection; they cannot fire synchronously at input-parse
time without holding up the turn.

### TUI input path

The TUI input widget (`zeph-tui`) passes the raw string through
`AppEvent::UserInput` → `agent.process_user_message`. Autocomplete suggestions
are currently slash-command-only. Adding `@name` autocomplete would require a
separate suggestion source (a name list from the sub-agent registry).

---

## 3. What Would Need to Change

| Layer | Change Required |
|---|---|
| `zeph-core/agent/mod.rs` | Add `@name` prefix detection before `dispatch_slash_command`; strip prefix, resolve target, forward remainder |
| Agent/sub-agent registry | New `AgentRegistry` type mapping `name → endpoint` (local preset or A2A URL); populated at startup from `[[acp.subagents.presets]]` and `[a2a]` peers |
| `zeph-tui` input widget | Add `@` autocomplete source from `AgentRegistry` (parallel to `/` slash-command autocomplete) |
| A2A dispatch path | Add synchronous-return path for local presets (fork `SubAgentManager`); keep async-return path for A2A remote agents, bridging result back into the TUI streaming panel |
| Config | New optional `[[agents.registry]]` section listing `name` + `endpoint` pairs |
| `dispatch_slash_command` | Ensure `@` prefix does not conflict with existing slash-command dispatch (trivially safe: slash commands begin with `/`) |

Estimated scope: **~400 LOC** plus TUI autocomplete changes and config migration.

---

## 4. Verdict: Defer

**Recommendation: do not implement in this PR. Track as a P4 enhancement.**

Rationale:

1. **Missing prerequisite** — a named agent registry does not exist. Adding
   mention routing without it would require hardcoding agent names, which is
   inflexible and inconsistent with the config-driven provider/skill pattern.

2. **Scope mismatch** — this PR is scoped to three UX gaps (#3308, #3314, #3327)
   that surface state already computed by Zeph. Mention routing requires new
   infrastructure (registry, A2A session management, TUI autocomplete).

3. **Low adoption signal** — Goose's `@extension` pattern works because
   extensions are always local and synchronous. Zeph's sub-agents can be remote
   A2A calls with non-trivial latency; the UX implication (blocking the turn vs.
   streaming a "connecting…" spinner) needs design work.

4. **The hook template (#3327 main ask) is already delivered** — the hook
   template in `[[hooks.turn_complete]]` and the `[notifications]` config give
   users the desktop-notification path without any mention routing.

**Suggested future action**: open a dedicated issue for `@agent` mention routing
after `AgentRegistry` infrastructure is in place (likely post-ACP subagent
stabilisation). Label: `enhancement`, `P4`, `tui`, `a2a`.

---

## Key Invariants (if implemented)

If mention routing is implemented in a future PR, the following invariants apply:

1. Unknown mention names MUST fall through to the default agent — never error.
2. The stripped message (without `@name`) MUST be forwarded to the target, not
   the full original text.
3. The full original message (with `@name`) MUST appear in conversation history
   for replay and recap correctness.
4. Mention routing MUST NOT bypass the security pre-screen (`pre_process_security`).
5. Remote A2A mention targets MUST apply the existing `a2a_seconds` timeout.
6. Mention routing MUST work consistently across CLI, TUI, and Telegram channels.
