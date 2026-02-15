# Channels

Zeph supports multiple I/O channels for interacting with the agent. Each channel implements the `Channel` trait (returning `Result<_, ChannelError>` with typed variants for I/O, closed-channel, and cancellation errors) and can be selected at runtime based on configuration or CLI flags.

## Available Channels

| Channel | Activation | Streaming | Confirmation |
|---------|-----------|-----------|--------------|
| **CLI** | Default (no config needed) | Token-by-token to stdout | y/N prompt |
| **Telegram** | `ZEPH_TELEGRAM_TOKEN` env var or `[telegram]` config | Edit-in-place every 10s | Reply "yes" to confirm |
| **TUI** | `--tui` flag or `ZEPH_TUI=true` (requires `tui` feature) | Real-time in chat panel | Auto-confirm (Phase 1) |

## CLI Channel

The default channel. Reads from stdin, writes to stdout with immediate streaming output.

```bash
./zeph
```

No configuration required. Supports all slash commands (`/skills`, `/mcp`, `/reset`).

## Telegram Channel

Run Zeph as a Telegram bot with streaming responses, MarkdownV2 formatting, and user whitelisting.

### Setup

1. Create a bot via [@BotFather](https://t.me/BotFather):
   - Send `/newbot` and follow the prompts
   - Copy the bot token (e.g., `123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11`)

2. Configure the token via environment variable or config file:

   ```bash
   # Environment variable
   ZEPH_TELEGRAM_TOKEN="123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11" ./zeph
   ```

   Or in `config/default.toml`:

   ```toml
   [telegram]
   allowed_users = ["your_username"]
   ```

   The token can also be stored in the age-encrypted vault:

   ```bash
   # Store in vault
   ZEPH_TELEGRAM_TOKEN=your-token
   ```

> The token is resolved via the vault provider (`ZEPH_TELEGRAM_TOKEN` secret). When using the `env` vault backend (default), set the environment variable directly. With the `age` backend, store it in the encrypted vault file.

### User Whitelisting

Restrict bot access to specific Telegram usernames:

```toml
[telegram]
allowed_users = ["alice", "bob"]
```

When `allowed_users` is empty, the bot accepts messages from all users. Messages from unauthorized users are silently rejected with a warning log.

### Bot Commands

| Command | Description |
|---------|-------------|
| `/start` | Welcome message |
| `/reset` | Reset conversation context |
| `/skills` | List loaded skills |

### Streaming Behavior

Telegram has API rate limits, so streaming works differently from CLI:

- First chunk sends a new message immediately
- Subsequent chunks edit the existing message in-place
- Updates are throttled to one edit per 10 seconds to respect Telegram rate limits
- On flush, a final edit delivers the complete response
- Long messages (>4096 chars) are automatically split into multiple messages

### MarkdownV2 Formatting

LLM responses are automatically converted from standard Markdown to Telegram's MarkdownV2 format. Code blocks, bold, italic, and inline code are preserved. Special characters are escaped to prevent formatting errors.

### Confirmation Prompts

When the agent needs user confirmation (e.g., destructive shell commands), Telegram sends a text prompt asking the user to reply "yes" to confirm.

## TUI Dashboard

A rich terminal interface based on ratatui with real-time agent metrics. Requires the `tui` feature flag.

```bash
cargo build --release --features tui
./zeph --tui
```

See [TUI Dashboard](tui.md) for full documentation including keybindings, layout, and architecture.

## Message Queueing

Zeph maintains a bounded FIFO message queue (maximum 10 messages) to handle user input received during model inference. Queue behavior varies by channel:

### CLI Channel

Blocking stdin read — the queue is always empty. CLI users cannot send messages while the agent is responding.

### Telegram Channel

New messages are queued via an internal mpsc channel. Consecutive messages arriving within 500ms are automatically merged with a newline separator to reduce context fragmentation.

Use `/clear-queue` to discard queued messages.

### TUI Channel

The input line remains interactive during model inference. Messages are queued in-order and drained after each response completes.

- **Queue badge:** `[+N queued]` appears in the input area when messages are pending
- **Clear queue:** Press `Ctrl+K` to discard all queued messages
- **Merging:** Consecutive messages within 500ms are merged by newline

When the queue is full (10 messages), new input is silently dropped until space becomes available.

## Channel Selection Logic

Zeph selects the channel at startup based on the following priority:

1. `--tui` flag or `ZEPH_TUI=true` → TUI channel (requires `tui` feature)
2. `ZEPH_TELEGRAM_TOKEN` set → Telegram channel
3. Otherwise → CLI channel

Only one channel is active per session.
