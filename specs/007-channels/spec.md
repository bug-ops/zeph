# Spec: Multi-Channel I/O

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
