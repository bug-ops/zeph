---
aliases:
  - Telegram Bot-to-Bot
  - Bot Communication
  - BotAccessSettings
tags:
  - sdd
  - spec
  - channels
  - telegram
  - bot-api-10
  - multi-agent
created: 2026-05-10
status: implemented
related:
  - "[[007-channels/spec]]"
  - "[[007-channels/007-1-telegram-guest-mode]]"
  - "[[001-system-invariants/spec]]"
  - "[[020-config-loading/spec]]"
---

# Spec: Telegram Bot-to-Bot Communication

> [!info]
> Telegram Bot API 10.0 enables bots to receive and respond to messages from
> other bots. This spec covers authorization, startup registration via
> `setManagedBotAccessSettings`, reply-chain loop prevention, and `is_from_bot`
> metadata propagation. Implemented in `zeph-channels`, `zeph-core`, `zeph-config`.
> Closes #3730.

## Sources

### External

- [Telegram Bot API 10.0 Changelog](https://core.telegram.org/bots/api#may-10-2025) — `BotAccessSettings`, `getManagedBotAccessSettings`, `setManagedBotAccessSettings`
- Depends on `TelegramApiExt` raw HTTP wrapper — see issue #3728

### Internal

| File | Contents |
|---|---|
| `crates/zeph-channels/src/telegram.rs` | `TelegramChannel`, authorization logic, startup sequence |
| `crates/zeph-channels/src/telegram_api_ext.rs` | `TelegramApiClient`, `set_managed_bot_access_settings` (from #3728) |
| `crates/zeph-core/src/channel.rs` | `ChannelMessage`, `IncomingMessage` types |
| `crates/zeph-config/src/telegram.rs` | `TelegramConfig` — `bot_to_bot`, `allowed_bots`, `max_bot_chain_depth` |

---

## 1. Overview

### Problem Statement

By default, Telegram bots cannot receive messages from other bots. Bot API 10.0
lifts this restriction via `setManagedBotAccessSettings`. Without implementing
this feature, Zeph cannot participate in multi-agent Telegram workflows where
autonomous agents collaborate in a shared chat. The primary risk of enabling
bot-to-bot communication is infinite reply loops when two bots each respond to
the other's messages.

### Goal

When `telegram.bot_to_bot = true`, Zeph calls `setManagedBotAccessSettings` at
startup, accepts messages from authorized bots, tracks reply-chain depth, and
drops responses when the chain exceeds `max_bot_chain_depth`. When
`bot_to_bot = false` (default), all messages from bots are silently ignored and
existing behavior is fully preserved.

### Out of Scope

- Orchestrating multi-agent task decomposition — Zeph processes bot messages
  the same way it processes user messages; coordination is the responsibility
  of the calling bot
- Implementing `TelegramApiExt` itself — covered by issue #3728
- Persistent agent registry or discovery for collaborating bots

---

## 2. User Stories

### US-001: Receive message from another bot

AS A multi-agent system operator,
I WANT Zeph to receive and process messages sent by authorized bots,
SO THAT multiple AI agents can collaborate in a shared Telegram chat.

**Acceptance criteria:**

```
GIVEN telegram.bot_to_bot = true
AND the sending bot's username is in allowed_bots (or allowed_bots is empty)
WHEN a bot sends a message in a chat where Zeph is active
THEN Zeph processes the message and responds normally
```

### US-002: Loop prevention

AS AN operator,
I WANT Zeph to stop replying when a reply chain grows too deep,
SO THAT two bots cannot enter an infinite reply loop.

**Acceptance criteria:**

```
GIVEN max_bot_chain_depth = 3
WHEN the reply chain depth of an incoming bot message equals or exceeds 3
THEN Zeph silently drops the message without sending any response
AND logs a warning with the chain depth and message ID
```

### US-003: Bot authorization

AS AN operator,
I WANT to restrict which bots can interact with Zeph,
SO THAT only trusted bots in allowed_bots can trigger responses.

**Acceptance criteria:**

```
GIVEN bot_to_bot = true
AND allowed_bots = ["@trusted_bot"]
WHEN a bot not in allowed_bots sends a message
THEN Zeph silently ignores the message (same as non-authorized user)
```

### US-004: Default behavior preserved

AS AN operator who has not opted in to bot-to-bot mode,
I WANT Zeph to ignore all bot-originated messages,
SO THAT the existing access control behavior is unchanged.

**Acceptance criteria:**

```
GIVEN telegram.bot_to_bot = false (default)
WHEN any bot sends a message
THEN Zeph drops the message without processing or logging an error
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN `telegram.bot_to_bot = false` THE SYSTEM SHALL treat messages from bots identically to how they were treated before Bot API 10.0: ignored | must |
| FR-002 | WHEN `telegram.bot_to_bot = true` THE SYSTEM SHALL call `setManagedBotAccessSettings` at channel startup before the dispatcher loop begins | must |
| FR-003 | WHEN `setManagedBotAccessSettings` fails at startup THE SYSTEM SHALL log a warning and continue; the failure SHALL NOT prevent the channel from starting | must |
| FR-004 | WHEN a message arrives from a sender where `User.is_bot = true` THE SYSTEM SHALL set `IncomingMessage.is_from_bot = true` | must |
| FR-005 | WHEN `is_from_bot = true` and `bot_to_bot = false` THE SYSTEM SHALL silently drop the message | must |
| FR-006 | WHEN `is_from_bot = true` and `bot_to_bot = true` THE SYSTEM SHALL check the sender's username against `allowed_bots`; if the list is non-empty and the sender is absent, drop the message | must |
| FR-007 | WHEN processing an authorized bot message THE SYSTEM SHALL compute the reply chain depth by walking `Message.reply_to_message` links | must |
| FR-008 | WHEN reply chain depth ≥ `max_bot_chain_depth` THE SYSTEM SHALL drop the message, log a warning with depth and message ID, and send no response | must |
| FR-009 | WHEN reply chain depth < `max_bot_chain_depth` THE SYSTEM SHALL process and respond to the bot message normally | must |
| FR-010 | WHEN `allowed_bots` is empty and `bot_to_bot = true` THE SYSTEM SHALL accept messages from all bots (no username restriction) | must |
| FR-011 | THE SYSTEM SHALL expose `is_from_bot: bool` on `ChannelMessage` so that skills and tool executors can branch on bot origin | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | Bot authorization check (`allowed_bots`) MUST execute before any LLM call or memory access |
| NFR-002 | Security | Loop prevention depth check MUST execute after authorization and before LLM call |
| NFR-003 | Reliability | `setManagedBotAccessSettings` failure at startup SHALL be non-fatal; dispatcher continues |
| NFR-004 | Observability | Dropped messages due to loop prevention SHALL be logged at `warn` level with chain depth, message ID, and sender username |
| NFR-005 | Performance | Reply chain depth calculation SHALL traverse at most `max_bot_chain_depth + 1` reply links; traversal SHALL NOT make additional API calls (use message payload only) |
| NFR-006 | Correctness | A bot messaging itself SHALL be detected by the depth check (self-reply creates depth ≥ 1 immediately) and dropped at the configured threshold |

---

## 5. Data Model Changes

### `TelegramConfig` (new fields)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bot_to_bot` | `bool` | `false` | Enable receiving messages from other bots |
| `allowed_bots` | `Vec<String>` | `[]` | Bot usernames allowed to interact; empty means all bots |
| `max_bot_chain_depth` | `u32` | `3` | Maximum reply chain depth before response is dropped |

### `IncomingMessage` (new field)

| Field | Type | Description |
|-------|------|-------------|
| `is_from_bot` | `bool` | `true` when the sender has `User.is_bot = true` |

### `ChannelMessage` (new metadata field)

| Field | Type | Description |
|-------|------|-------------|
| `is_from_bot` | `bool` | Forwarded from `IncomingMessage`; available to skills and tool executors |

### New Telegram API types (in `telegram_api_ext.rs`, from #3728)

| Type | Fields | Description |
|------|--------|-------------|
| `BotAccessSettings` | `allow_user_messages: bool`, `allow_bot_messages: bool` | Current bot communication settings |

---

## 6. Loop Prevention Algorithm

The reply chain depth is computed from message metadata available in the incoming
`Update` payload. No additional API calls are made.

```
fn compute_chain_depth(message: &Message) -> u32:
    depth = 0
    current = message.reply_to_message
    while current is Some AND depth < max_bot_chain_depth + 1:
        depth += 1
        current = current.reply_to_message
    return depth
```

Decision table:

| depth | Condition | Action |
|-------|-----------|--------|
| 0 | Message is not a reply | Process normally |
| 1 .. max-1 | Within limit | Process normally |
| ≥ max | At or beyond limit | Drop + warn log |

> [!warning]
> The traversal cap of `max_bot_chain_depth + 1` is intentional — it prevents
> unbounded traversal when the reply chain is long. Once the cap is reached,
> the message is dropped regardless of whether the full chain was walked.

> [!danger]
> NEVER walk the full reply chain without a depth cap — malicious actors could
> craft artificially deep reply chains to cause unbounded processing.

---

## 7. Startup Behavior

When `bot_to_bot = true`, the channel startup sequence calls
`TelegramApiClient::set_managed_bot_access_settings` before entering the
dispatcher loop:

```
TelegramChannel::start():
    if config.bot_to_bot:
        result = api_client.set_managed_bot_access_settings(
            BotAccessSettings { allow_user_messages: true, allow_bot_messages: true }
        )
        if result is Err:
            warn!("set_managed_bot_access_settings failed: {}; continuing", err)
    start_dispatcher_loop()
```

The registration is idempotent on the Telegram side — re-running `setManagedBotAccessSettings`
with the same settings has no effect.

---

## 8. Authorization Flow

```
incoming message received
    │
    ├─ is_from_bot = false → existing user authorization path (unchanged)
    │
    └─ is_from_bot = true
           │
           ├─ bot_to_bot = false → SILENT DROP (no log)
           │
           └─ bot_to_bot = true
                  │
                  ├─ allowed_bots non-empty AND sender NOT in list → SILENT DROP
                  │
                  └─ authorized
                         │
                         ├─ chain_depth ≥ max_bot_chain_depth → WARN DROP
                         │
                         └─ chain_depth < max_bot_chain_depth → PROCESS
```

> [!danger]
> NEVER process a bot message without completing both the `allowed_bots` check
> AND the loop prevention depth check.
> NEVER call `setManagedBotAccessSettings` when `bot_to_bot = false`.

---

## 9. Config Schema

```toml
[telegram]
# Enable receiving messages from other Telegram bots (Bot API 10.0).
# When false (default), all bot-originated messages are silently ignored.
bot_to_bot = false

# Bot usernames allowed to send messages to Zeph when bot_to_bot = true.
# Empty list (default) allows all bots. Entries should include the @ prefix.
# Example: allowed_bots = ["@my_orchestrator_bot", "@pipeline_bot"]
allowed_bots = []

# Maximum reply chain depth before Zeph stops responding.
# Prevents infinite loops between bots. Default: 3.
max_bot_chain_depth = 3
```

---

## 10. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Bot sends to itself (self-reply) | Reply chain depth ≥ 1; dropped at threshold |
| Two bots in loop, one hits depth limit | The bot reaching depth ≥ max drops; loop terminates |
| `setManagedBotAccessSettings` returns HTTP 429 | Log warning with retry-after value; do not retry automatically; continue startup |
| `allowed_bots` contains a username without `@` prefix | Treat as-is; document that `@` prefix is convention, not required by the check |
| `max_bot_chain_depth = 0` | Every bot message is dropped immediately (no bot replies ever sent) |
| `max_bot_chain_depth = 1` | Only direct (non-reply) bot messages are processed |
| Bot message has no text | Process as empty message (same as user empty message behavior) |
| `bot_to_bot` toggled to `false` at runtime | New bot messages dropped; in-flight responses complete normally |

---

## 11. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `bot_to_bot = false` (default): zero bot messages processed | 100% |
| SC-002 | `allowed_bots` check executes before LLM call for 100% of bot messages | 100% |
| SC-003 | Loop prevention fires at exactly `max_bot_chain_depth` (unit test: depth N-1 processes, depth N drops) | 100% |
| SC-004 | `setManagedBotAccessSettings` called once at startup when `bot_to_bot = true` | Verified by unit test |
| SC-005 | Live test: two bot instances exchange messages; loop terminates after `max_bot_chain_depth` responses | Pass |

---

## 12. Key Invariants

- `bot_to_bot = false` is the hard default — operators must opt in explicitly
- Bot authorization (`allowed_bots`) runs unconditionally before any LLM invocation
- Loop prevention depth check runs after authorization, before LLM invocation
- Reply chain traversal is bounded by `max_bot_chain_depth + 1` iterations
- `setManagedBotAccessSettings` failure at startup is non-fatal
- `is_from_bot` on `ChannelMessage` is set from message metadata, not inferred

---

## 13. NEVER

- NEVER process a bot message when `bot_to_bot = false`, regardless of other config
- NEVER skip the `allowed_bots` check when `bot_to_bot = true`
- NEVER skip the loop prevention check for authorized bots
- NEVER traverse the reply chain without a depth cap
- NEVER call `setManagedBotAccessSettings` when `bot_to_bot = false`
- NEVER block the dispatcher startup on `setManagedBotAccessSettings` success
- NEVER send a response when chain depth ≥ `max_bot_chain_depth`

---

## 14. Agent Boundaries

### Always (without asking)

- Run `cargo nextest` after changes to authorization logic or startup sequence
- Follow existing authorization patterns in `TelegramChannel`
- Add `///` doc comments to all new public types and methods

### Ask First

- Changing `IncomingMessage` or `ChannelMessage` structs (shared across channels)
- Adding new fields to `TelegramConfig` beyond what is specified here
- Changing the startup call sequence for `TelegramChannel`

### Never

- Introduce blocking I/O in the dispatcher async loop
- Make additional Telegram API calls to walk reply chains beyond the message payload
- Skip `allowed_bots` check in any code path where `is_from_bot = true`

---

## 15. Acceptance Criteria (Issue #3730) — Implemented in PR #3748

- [x] `bot_to_bot = false` (default): bot ignores messages from other bots (existing behavior preserved)
- [x] `bot_to_bot = true`: bot responds to bots in `allowed_bots` (or all bots if list is empty)
- [x] `setManagedBotAccessSettings` called at startup when `bot_to_bot = true`
- [x] `setManagedBotAccessSettings` NOT called when `bot_to_bot = false`
- [x] Reply chain depth tracked; responses dropped when depth ≥ `max_bot_chain_depth`
- [x] Warn log emitted when message dropped due to depth limit (includes depth, message ID, sender)
- [x] `is_from_bot` field available on `ChannelMessage`
- [x] `is_from_bot` field available on `IncomingMessage`
- [x] Unit tests: `bot_to_bot = false` drops, authorization pass, authorization fail, depth 0 processes, depth N-1 processes, depth N drops, `max_bot_chain_depth = 0` drops all
- [ ] Live test: two bot instances communicate; loop terminates after `max_bot_chain_depth` (pending live session)
- [ ] Playbook updated: `.local/testing/playbooks/telegram.md`
- [ ] Coverage-status updated

---

## 16. References

- Issue #3730 — this feature
- Issue #3726 — Telegram Bot API 10.0 epic
- Issue #3728 — `TelegramApiExt` raw HTTP wrapper (dependency)
- [[007-channels/spec]] — channel trait, AnyChannel, streaming protocol
- [[007-channels/007-1-telegram-guest-mode]] — related Bot API 10.0 feature
- [[001-system-invariants/spec]] — cross-cutting architectural invariants
- [[020-config-loading/spec]] — config resolution order, defaults
