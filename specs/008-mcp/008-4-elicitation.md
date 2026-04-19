---
aliases:
  - MCP Elicitation
  - Server-Driven Elicitation
tags:
  - sdd
  - spec
  - mcp
  - ux
  - channels
created: 2026-04-19
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[008-mcp/spec]]"
  - "[[007-channels/spec]]"
  - "[[047-cli-modes/spec]]"
  - "[[010-security/spec]]"
---

# Spec: MCP Server-Driven Elicitation

> [!info]
> MCP servers speaking protocol 2025-06-18 can pause a tool call and request
> structured user input. Requests are routed to the active channel. Sensitive
> fields trigger a user warning. Landed in #3218.

## Sources

### External
- **MCP protocol 2025-06-18** — `elicitation/create` RPC added to the spec
- Deadlock regression context: `#2542` (mpsc drain path)

### Internal

| File | Contents |
|---|---|
| `crates/zeph-mcp/src/elicitation.rs` | Elicitation request routing |
| `crates/zeph-core/src/channel.rs` | `ElicitationRequest`, `ElicitationResponse` types |
| `crates/zeph-channels/src/cli.rs` | CLI elicitation handling (interactive prompt) |
| `crates/zeph-channels/src/tui_channel.rs` | TUI elicitation (status modal) |
| `crates/zeph-channels/src/telegram.rs` | Telegram elicitation (message + timeout) |
| `crates/zeph-channels/src/json_cli.rs` | JSON mode elicitation (`elicitation` event) |

---

## 1. Overview

### Problem Statement

MCP servers sometimes need structured user input mid-tool-call (e.g., OAuth
authorization code, password for a protected resource, confirmation of a
destructive action). Without elicitation support, these servers would either
fail silently or require out-of-band communication. Protocol version 2025-06-18
adds a first-class `elicitation/create` RPC for this purpose.

### Goal

Route MCP elicitation requests to the active channel so that:

- CLI mode presents an interactive prompt
- TUI mode shows a status modal
- Telegram mode sends a message and waits for a reply (with configurable timeout)
- JSON mode emits an `elicitation` event and reads a response line from stdin
- Sandboxed sessions reject elicitation entirely

### Out of Scope

- URL-type field handling (declined in phase 1; planned for a future iteration)
- Storing elicitation responses in memory
- Elicitation from non-MCP sources (agent-generated confirmations use `confirm()`)

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN an MCP server at protocol 2025-06-18 sends `elicitation/create` THE SYSTEM SHALL route it to the active channel's `elicit()` method | must |
| FR-002 | WHEN the trust level is `Sandboxed` THE SYSTEM SHALL reject elicitation requests with a structured error response — never prompt the user | must |
| FR-003 | WHEN a field has type `password`, `token`, or `key` THE SYSTEM SHALL display a warning to the user before presenting the prompt | must |
| FR-004 | WHEN a field has type `url` THE SYSTEM SHALL decline the field with `declined: true` in the response (phase 1 limitation) | must |
| FR-005 | Field `name` values SHALL be sanitized at display time to remove ANSI escape codes and Markdown injection vectors; raw names are preserved for the MCP response round-trip | must |
| FR-006 | WHEN Telegram elicitation is used THE SYSTEM SHALL send a message and await a reply within the configured `telegram_elicitation_timeout_secs` | must |
| FR-007 | WHEN the Telegram timeout expires THE SYSTEM SHALL return a `declined` response | must |
| FR-008 | THE SYSTEM SHALL include a deadlock regression test verifying the mpsc drain path does not block when the channel is full (#2542) | must |
| FR-009 | ANSI sanitization unit tests SHALL cover at least: bare ESC code, `\x1b[31m` color, `\x1b[0m` reset, and a benign display name | must |

---

## 3. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | Sandboxed trust level MUST hard-reject elicitation — no prompt, no channel call |
| NFR-002 | Security | Sensitive field type warning is mandatory before the prompt; cannot be suppressed by config |
| NFR-003 | Security | Field names are sanitized at display time only; MCP round-trip uses raw names |
| NFR-004 | Reliability | Telegram elicitation timeout prevents indefinite blocking; default 60 s |
| NFR-005 | Reliability | Deadlock-free mpsc drain path verified by regression test |
| NFR-006 | Usability | Elicitation in JSON mode emits an `{"type":"elicitation",...}` event and reads the response from the next stdin line |

---

## 4. Key Invariants

### Always (without asking)
- Sandboxed trust level never prompts the user — hard reject with error response
- Sensitive field warning is displayed before the prompt, not after
- Raw field names preserved for MCP response; sanitized names for display only
- URL-type fields are declined, not prompted

### Ask First
- Enabling URL-type field prompting (security review required)
- Disabling the sensitive-field warning for specific trust levels
- Extending elicitation to non-MCP agent-generated prompts

### Never
- Prompt the user for elicitation in Sandboxed mode
- Log raw elicitation responses (they may contain passwords or tokens)
- Use sanitized field names in the MCP response (round-trip must use raw names)

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| MCP server at pre-2025-06-18 protocol sends elicitation | Request is unrecognized; return JSON-RPC method-not-found error |
| Sandboxed session receives elicitation | Return structured `rejected` response; no channel call |
| Telegram timeout expires | Return `declined` response to MCP server; log at INFO |
| mpsc channel full during elicitation drain | Drain path yields without blocking; regression test #2542 covers this |
| Field name contains `\x1b[31m` | Display shows `[31m` stripped; raw name sent in response |
| Field type = url | `declined: true` in the field response; user not prompted |

---

## 6. Acceptance Criteria

```
GIVEN an MCP server at protocol 2025-06-18
  AND trust level = Default
WHEN the server sends elicitation/create with a password field
THEN a sensitive-field warning is displayed to the user
AND the user is prompted for the password
AND the response is returned to the MCP server with the raw field name

GIVEN trust level = Sandboxed
WHEN the server sends elicitation/create
THEN the request is rejected with a structured error
AND the user is never prompted
AND no channel elicit() call is made

GIVEN Telegram channel with telegram_elicitation_timeout_secs = 30
  AND the user does not reply within 30 s
WHEN elicitation times out
THEN declined response is sent to the MCP server
AND the tool call continues (or fails per MCP server logic)
```

---

## 7. See Also

- [[008-mcp/spec]] — MCP client parent spec
- [[010-security/spec]] — trust levels
- [[007-channels/spec]] — channel trait and `elicit()` method
- [[047-cli-modes/spec]] — JSON mode elicitation event schema
- [[MOC-specs]] — all specifications
