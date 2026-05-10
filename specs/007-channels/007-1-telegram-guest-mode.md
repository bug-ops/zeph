---
aliases:
  - Telegram Guest Mode
  - Guest Query
  - answerGuestQuery
tags:
  - sdd
  - spec
  - channels
  - telegram
  - bot-api-10
created: 2026-05-10
status: implemented
related:
  - "[[007-channels/spec]]"
  - "[[007-channels/007-2-telegram-bot-to-bot]]"
  - "[[001-system-invariants/spec]]"
  - "[[020-config-loading/spec]]"
---

# Spec: Telegram Guest Mode

> [!info]
> Telegram Bot API 10.0 allows bots to receive `guest_message` updates when
> @mentioned in any chat without membership. Zeph handles these updates via a
> new dispatch branch, routes responses through `answerGuestQuery`, enforces
> access control against `allowed_users`, and constrains streaming to a single
> response. Implemented in `zeph-channels`, `zeph-core`, `zeph-config`.
> Closes #3729.

## Sources

### External

- [Telegram Bot API 10.0 Changelog](https://core.telegram.org/bots/api#may-10-2025) — `guest_message`, `answerGuestQuery`, `guest_bot_caller_user`, `guest_bot_caller_chat`, `guest_query_id`, `User.supports_guest_queries`, `SentGuestMessage`
- Depends on `TelegramApiExt` raw HTTP wrapper — see issue #3728

### Internal

| File | Contents |
|---|---|
| `crates/zeph-channels/src/telegram.rs` | `TelegramChannel`, dispatcher, streaming logic |
| `crates/zeph-channels/src/telegram_api_ext.rs` | `TelegramApiClient`, `answer_guest_query` (from #3728) |
| `crates/zeph-core/src/channel.rs` | `ChannelMessage`, `IncomingMessage` types |
| `crates/zeph-config/src/telegram.rs` | `TelegramConfig` — `guest_mode` field lives here |

---

## 1. Overview

### Problem Statement

Zeph (as a Telegram bot) can only interact with users who add it to a chat or
message it in a private conversation. Telegram Bot API 10.0 lifts this restriction
by letting any user @mention a bot in any chat. The bot receives a `guest_message`
update and responds via `answerGuestQuery`. Without implementing this, Zeph misses
a zero-friction interaction mode that lowers the adoption barrier significantly.

### Goal

When `telegram.guest_mode = true`, Zeph processes `guest_message` updates, verifies
the calling user against `allowed_users`, and sends exactly one response per guest
query via `answerGuestQuery`. When `guest_mode = false` (default), guest updates are
silently ignored and existing behavior is fully preserved.

### Out of Scope

- Editing or streaming partial responses in guest context (`editMessageText` is not
  available for guest replies; the single-response constraint is architectural)
- Persistent multi-turn conversations in guest context (only the tagged message is
  visible to the bot; no prior context is available)
- Reply-thread continuation from within the guest chat
- Implementing `TelegramApiExt` itself — that is covered by issue #3728

---

## 2. User Stories

### US-001: @mention response

AS A user who @mentions Zeph in a group chat,
I WANT Zeph to respond to my tagged message,
SO THAT I can get AI assistance without adding the bot to the chat.

**Acceptance criteria:**

```
GIVEN telegram.guest_mode = true
AND the user is in allowed_users (or allowed_users is empty)
WHEN the user @mentions Zeph in any Telegram chat
THEN Zeph processes the tagged message and responds via answerGuestQuery within
     the same session, with a system prompt annotation indicating guest context
```

### US-002: Access control enforcement

AS AN operator,
I WANT only users in `allowed_users` to trigger guest responses,
SO THAT unauthorized users cannot invoke the bot in arbitrary chats.

**Acceptance criteria:**

```
GIVEN telegram.guest_mode = true
AND allowed_users is non-empty
WHEN a user not in allowed_users sends a guest_message
THEN Zeph silently ignores the update (no response, no error)
```

### US-003: Feature disabled by default

AS AN operator who has not opted in to guest mode,
I WANT Zeph to ignore guest_message updates,
SO THAT existing behavior and access patterns are fully preserved.

**Acceptance criteria:**

```
GIVEN telegram.guest_mode = false (default)
WHEN any guest_message update is received
THEN Zeph drops the update without processing or logging an error
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN `telegram.guest_mode = false` THE SYSTEM SHALL ignore all `guest_message` updates without error | must |
| FR-002 | WHEN `telegram.guest_mode = true` THE SYSTEM SHALL register a dispatcher handler for the `guest_message` update type | must |
| FR-003 | WHEN a `guest_message` update is received THE SYSTEM SHALL extract `guest_bot_caller_user` and check it against `allowed_users`; if the check fails, the update SHALL be silently dropped | must |
| FR-004 | WHEN `allowed_users` is empty and `guest_mode = true` THE SYSTEM SHALL accept guest messages from any user | must |
| FR-005 | WHEN processing a guest message THE SYSTEM SHALL set `guest_query_id` on `IncomingMessage` to the opaque ID from the update | must |
| FR-006 | WHEN `guest_query_id` is present on the outbound message THE SYSTEM SHALL route the response through `TelegramApiClient::answer_guest_query` instead of `send_message` / `edit_message_text` | must |
| FR-007 | WHEN in guest context THE SYSTEM SHALL accumulate the full LLM response before sending (no streaming edits) | must |
| FR-008 | WHEN in guest context THE SYSTEM SHALL prepend a system prompt annotation: "You were @mentioned in a chat. You can only see the message that tagged you. Do not reference prior context." | must |
| FR-009 | WHEN a guest interaction completes THE SYSTEM SHALL store the exchange in memory with `context: "guest"` label | should |
| FR-010 | WHEN `telegram.guest_mode` is not present in config THE SYSTEM SHALL default to `false` | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | `allowed_users` check against `guest_bot_caller_user` MUST run before any LLM call or memory access |
| NFR-002 | Reliability | A failure in `answer_guest_query` HTTP call SHALL be logged as a warning and not crash the dispatcher loop |
| NFR-003 | Performance | Guest message processing (auth check + LLM + `answerGuestQuery`) SHALL complete within the same latency envelope as regular messages (no additional blocking layers) |
| NFR-004 | Observability | Failed authorization attempts for guest messages SHALL be logged at `warn` level with the caller's user ID |
| NFR-005 | Correctness | The `send_message` / `edit_message_text` code path MUST NOT be called for a message with a non-None `guest_query_id` |

---

## 5. Data Model Changes

### `TelegramConfig` (new field)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `guest_mode` | `bool` | `false` | Enable guest message handling |

### `IncomingMessage` (new field)

| Field | Type | Description |
|-------|------|-------------|
| `guest_query_id` | `Option<String>` | Opaque ID from `guest_message` update; `None` for regular messages |

### `ChannelMessage` (new metadata field)

| Field | Type | Description |
|-------|------|-------------|
| `is_guest_context` | `bool` | `true` when the message originated from a guest mention |

### New Telegram API types (in `telegram_api_ext.rs`, from #3728)

| Type | Fields | Description |
|------|--------|-------------|
| `GuestMessage` | `guest_query_id: String`, `text: Option<String>`, `guest_bot_caller_user: User`, `guest_bot_caller_chat: Chat` | Subset of the `guest_message` update payload |
| `SentGuestMessage` | `guest_query_id: String`, `message: Message` | Response returned by `answerGuestQuery` |

---

## 6. Response Routing

The routing decision happens in `TelegramChannel::send` / `send_chunk`:

```
if incoming.guest_query_id.is_some():
    accumulate all chunks → full_text
    TelegramApiClient::answer_guest_query(query_id, full_text, ParseMode::Html)
else:
    existing send_message / edit_message_text path
```

> [!warning]
> `answerGuestQuery` is a one-shot call. There is no equivalent of `editMessageText`
> for guest responses. The implementation MUST buffer the complete response before
> calling the API, even when the LLM is streaming.

---

## 7. Streaming Behavior in Guest Context

| Behavior | Regular message | Guest message |
|----------|----------------|---------------|
| `send_typing` | Telegram typing indicator | Telegram typing indicator (allowed) |
| `send_chunk` | Buffered, sent as edit every `stream_interval_ms` | Buffered in memory only, no API call |
| `send` (final) | `editMessageText` or `sendMessage` | `answerGuestQuery` (single call) |
| Multi-turn | Supported | Not supported; only the tagged message is visible |

---

## 8. Access Control Flow

```
guest_message received
    │
    ├─ guest_mode = false → DROP (no log)
    │
    └─ guest_mode = true
           │
           ├─ allowed_users non-empty AND caller NOT in list → WARN log, DROP
           │
           └─ authorized → process message
                               │
                               └─ system prompt: guest context annotation
                               └─ LLM inference
                               └─ answer_guest_query(full response)
```

> [!danger]
> NEVER call `answer_guest_query` before the `allowed_users` check completes.
> NEVER call `send_message` or `edit_message_text` when `guest_query_id` is set.
> NEVER process a guest update when `guest_mode = false`, regardless of other config.

---

## 9. Config Schema

```toml
[telegram]
# Enable bot to respond when @mentioned in any Telegram chat (Bot API 10.0).
# When false (default), guest_message updates are ignored entirely.
guest_mode = false
```

The `allowed_users` field already present in `TelegramConfig` is reused for
guest-message authorization — no new field is needed.

---

## 10. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `guest_query_id` present but `answer_guest_query` returns HTTP 400 | Log warning with query ID; do not retry; do not fall back to `send_message` |
| `guest_message` update has empty `text` | Treat as empty user message; pass through to LLM with guest context annotation |
| `allowed_users` check fails | Silent drop + `warn!` log; no response sent to caller |
| LLM returns an error during guest processing | Log error; no response sent (callers expect one-shot responses, not error messages) |
| Multiple concurrent guest messages from the same user | Each processed independently; no ordering guarantee |
| `guest_mode` toggled to `false` at runtime (config reload) | New guest updates dropped; in-flight responses complete normally |

---

## 11. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `allowed_users` check executes before LLM call for 100% of guest updates | 100% |
| SC-002 | `send_message` / `edit_message_text` never called when `guest_query_id` is set | 0 violations (unit test enforced) |
| SC-003 | Unauthorized guest messages produce no response | 100% |
| SC-004 | Default config (`guest_mode = false`) ignores all guest updates | 100% |
| SC-005 | Live test: @mention in a group chat produces a response via `answerGuestQuery` | Pass |

---

## 12. Key Invariants

- `guest_mode = false` is the hard default — operators must opt in explicitly
- Access control (`allowed_users`) runs unconditionally before any LLM invocation
- Response routing is determined solely by the presence of `guest_query_id` on `IncomingMessage`
- The `answer_guest_query` path NEVER uses `editMessageText`
- The regular `send_message` path NEVER uses `answer_guest_query`
- Guest context annotation in system prompt is always prepended when `is_guest_context = true`
- Memory storage of guest interactions is labeled with `context: "guest"` to enable future filtering

---

## 13. NEVER

- NEVER respond to a guest message without first verifying `allowed_users`
- NEVER call `answer_guest_query` without `guest_query_id` being set
- NEVER call `editMessageText` for a guest response
- NEVER enable guest mode silently — it must be an explicit opt-in in config
- NEVER fall back to `send_message` when `answer_guest_query` fails
- NEVER expose prior conversation history in the guest context system prompt

---

## 14. Agent Boundaries

### Always (without asking)

- Run `cargo nextest` after changes to the dispatcher or routing logic
- Follow existing `TelegramChannel` code patterns for handler registration
- Add `///` doc comments to all new public types and methods

### Ask First

- Changing the `IncomingMessage` or `ChannelMessage` struct (shared across channels)
- Adding new fields to `TelegramConfig` beyond what is specified here

### Never

- Introduce blocking I/O in the dispatcher async loop
- Call `answer_guest_query` without verifying the access control result first
- Modify the teloxide dispatcher setup outside the Telegram channel module

---

## 15. Acceptance Criteria (Issue #3729) — Implemented in PR #3748

- [x] `telegram.guest_mode = false` disables the handler; bot ignores guest mentions
- [x] `telegram.guest_mode = true` registers handler; bot responds to @mentions
- [x] `allowed_users` check applied to `guest_bot_caller_user` before LLM call
- [x] Response routed through `answerGuestQuery`, not `sendMessage`
- [x] No `editMessageText` calls when `guest_query_id` is present
- [x] System prompt annotated with guest context string
- [x] `is_guest_context: bool` available on `ChannelMessage`
- [x] `guest_query_id: Option<String>` added to `IncomingMessage`
- [x] Unit tests: authorization pass, authorization fail, routing, config parsing
- [ ] Live test: @mention bot in a group, verify response reaches caller (pending live session)
- [ ] Playbook updated: `.local/testing/playbooks/telegram.md`
- [ ] Coverage-status updated

---

## 16. References

- Issue #3729 — this feature
- Issue #3726 — Telegram Bot API 10.0 epic
- Issue #3728 — `TelegramApiExt` raw HTTP wrapper (dependency)
- [[007-channels/spec]] — channel trait, AnyChannel, streaming protocol
- [[007-channels/007-2-telegram-bot-to-bot]] — related Bot API 10.0 feature
- [[001-system-invariants/spec]] — cross-cutting architectural invariants
- [[020-config-loading/spec]] — config resolution order, defaults
