---
aliases:
  - Multi-Channel I/O
  - Channel System
  - AnyChannel
tags:
  - sdd
  - spec
  - channels
  - io
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec#1. Channel Contract]]"
  - "[[011-tui/spec]]"
---

# Spec: Multi-Channel I/O

> [!info]
> Channel trait, AnyChannel dispatch, streaming support, feature parity across channels
> (CLI, Telegram, TUI, Discord, Slack); single I/O boundary for all I/O modes.

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-core/src/channel.rs` | `Channel` trait, `ChannelMessage`, `ChannelError` |
| `crates/zeph-channels/src/any.rs` | `AnyChannel` enum, `dispatch_channel!` macro |
| `crates/zeph-tui/src/channel.rs` | `TuiChannel` implementation |

---

`crates/zeph-channels/` — channel implementations and dispatch.

## Channel Trait

```rust
trait Channel: Send {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError>;
    async fn send(&mut self, text: &str) -> Result<(), ChannelError>;
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError>;
    async fn send_typing(&mut self) -> Result<(), ChannelError>;
    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError>;
    async fn send_tool_start(&mut self, event: ToolStartEvent<'_>) -> Result<(), ChannelError>;
    async fn send_tool_output(&mut self, event: ToolOutputEvent<'_>) -> Result<(), ChannelError>;
    fn supports_exit(&self) -> bool;
    // + additional methods for metadata, context, etc.
}
```

- `&mut self` — stateful, owned by Agent, single concurrent user
- Native Edition 2024 async — no `async-trait` macro
- `recv()` returns `None` on clean disconnect (EOF / user exit)
- `supports_exit()`: `false` for persistent channels (Telegram — server keeps running), `true` for ephemeral (CLI)

## AnyChannel Enum

```rust
pub enum AnyChannel {
    Cli(CliChannel),
    Telegram(TelegramChannel),
    #[cfg(feature = "discord")] Discord(DiscordChannel),
    #[cfg(feature = "slack")]   Slack(SlackChannel),
    #[cfg(feature = "tui")]     Tui(TuiChannel),
}
```

- Macro dispatch: `dispatch_channel!(self, method, args...)`
- **Only place** where multi-channel dispatch happens — no other dyn dispatch for channels
- New channels: add variant + `#[cfg(feature = "...")]` + macro dispatch entry

## Implementations

| Channel | Transport | Notes |
|---|---|---|
| `CliChannel` | stdin/stdout | Streaming via `send_chunk`; supports `/exit` |
| `TelegramChannel` | teloxide (Bot API) | Streaming via message edits; persistent (no exit) |
| `DiscordChannel` | discord HTTP | Optional (`discord` feature) |
| `SlackChannel` | Slack Events API | Optional (`slack` feature) |
| `TuiChannel` | ratatui/crossterm | TUI dashboard; owns stdin/stdout — conflicts with ACP stdio |

## Streaming Protocol

1. `send_typing()` — show typing indicator before LLM starts
2. `send_chunk(chunk)` — stream tokens as they arrive from LLM
3. `send(final_text)` — replace / finalize the streamed message
4. `send_tool_start(event)` — notify channel that tool execution begins
5. `send_tool_output(event)` — deliver tool result to channel

## Key Invariants

- Channel is always owned by the Agent — never shared via `Arc`
- `TuiChannel` and ACP stdio transport are **mutually exclusive** — both own stdin/stdout; enforced at startup
- Telegram channel must handle Telegram rate limits internally — agent loop must not see rate-limit errors as fatal
- MCP child process stderr must be suppressed when using `TuiChannel`
- `send_chunk` and `send` both must be implemented — streaming fallback is not acceptable for CLI

---

## Channel Feature Parity

Epic #1978. `crates/zeph-channels/`, `crates/zeph-core/src/channel.rs`.

### Overview

Channel feature parity ensures all `AnyChannel` variants and `AppChannel` forward every method defined in the `Channel` trait. Previously, four methods fell through to no-op trait defaults in some dispatch paths, silently dropping events. The parity initiative enforces full method forwarding and behavioral consistency across channels.

### Methods That Must Be Forwarded

The `Channel` trait defines 16 methods. All must be explicitly dispatched in `AnyChannel` and `AppChannel`:

Previously dropped (CHAN-01 fix):
- `send_thinking_chunk` — streams extended thinking tokens
- `send_stop_hint` — signals LLM stop reason to channel
- `send_usage` — delivers token usage stats to channel
- `send_tool_start` — notifies channel of tool execution start

These four now have explicit dispatch in `AnyChannel` and `AppChannel`, matching the existing dispatch for `send`, `send_chunk`, `send_typing`, `send_status`, `send_tool_output`, `recv`, `supports_exit`, and others.

### Timeout Consistency (CHAN-02)

All channel `confirm()` implementations must deny after 30 seconds (matching Telegram behavior). Previously, Discord and Slack `confirm()` blocked indefinitely. Shared `CONFIRM_TIMEOUT` constant (30s) defined in `zeph-channels` crate; all three implementations reference it.

### Discord Slash Commands (CHAN-05)

Discord channel registers slash commands (`/reset`, `/skills`, `/agent`) at startup via fire-and-forget background task. Uses `PUT /applications/{id}/commands` (idempotent). Failure is non-fatal.

### Channel Capability Matrix

| Method | CLI | Telegram | Discord | Slack | TUI |
|---|---|---|---|---|---|
| `send` | Full | Full | Full | Full | Full |
| `send_chunk` | Streaming | Batched (1s/512B debounce) | Supported | Supported | Full |
| `send_typing` | No-op | Bot typing indicator | No-op | No-op | Spinner |
| `send_status` | Inline text | No-op | No-op | No-op | Status bar |
| `send_tool_start` | Forwarded | Forwarded | Forwarded | Forwarded | Spinner |
| `send_tool_output` | Forwarded | Forwarded | Forwarded | Forwarded | Forwarded |
| `send_thinking_chunk` | Forwarded | Forwarded | Forwarded | Forwarded | Forwarded |
| `confirm` | Interactive | Inline button (30s timeout) | Slash cmd (30s timeout) | Interactive (30s timeout) | Dialog |

### Key Invariants

- `AnyChannel` `dispatch_channel!` macro must include ALL 16 `Channel` trait methods — no method may fall through to a default
- `CONFIRM_TIMEOUT` (30s) is the canonical timeout for all channel `confirm()` implementations — never hardcode different values per channel
- Discord slash command registration is fire-and-forget — startup must not fail if registration fails
- `send_thinking_chunk` must be forwarded even if the channel renders it as a no-op — the event must reach the channel impl
- NEVER add a new `Channel` trait method without updating `AnyChannel`, `AppChannel`, and all channel implementations
- Behavioral differences between channels (e.g. Telegram batching) are acceptable — method dropping is not
