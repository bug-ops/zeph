# Channels

Zeph supports multiple I/O channels for interacting with the agent. Each channel implements the `Channel` trait (returning `Result<_, ChannelError>` with typed variants for I/O, closed-channel, and cancellation errors) and can be selected at runtime based on configuration or CLI flags.

## Available Channels

| Channel | Activation | Streaming | Confirmation |
|---------|-----------|-----------|--------------|
| **CLI** | Default (no config needed) | Token-by-token to stdout | y/N prompt |
| **Discord** | `ZEPH_DISCORD_TOKEN` env var or `[discord]` config (requires `discord` feature) | Edit-in-place every 1.5s | Reply "yes" to confirm |
| **Slack** | `ZEPH_SLACK_BOT_TOKEN` env var or `[slack]` config (requires `slack` feature) | `chat.update` every 2s | Reply "yes" to confirm |
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

The `allowed_users` list **must not be empty**. The Telegram channel refuses to start without at least one allowed username to prevent accidentally exposing the bot to all users. Messages from unauthorized users are silently rejected with a warning log.

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

## Discord Channel

Run Zeph as a Discord bot with Gateway v10 WebSocket, slash commands, and edit-in-place streaming. Requires the `discord` feature flag.

```bash
cargo build --release --features discord
```

### Setup

1. Create an application at the [Discord Developer Portal](https://discord.com/developers/applications).
2. Under **Bot**, copy the bot token.
3. Under **OAuth2 > URL Generator**, select `bot` and `applications.commands` scopes, then invite the bot to your server.

4. Configure the token and application ID:

   ```bash
   ZEPH_DISCORD_TOKEN="your-bot-token" ZEPH_DISCORD_APP_ID="123456789" ./zeph
   ```

   Or in `config/default.toml`:

   ```toml
   [discord]
   token = "your-bot-token"
   application_id = "123456789"
   allowed_user_ids = []
   allowed_role_ids = []
   allowed_channel_ids = []
   ```

> Tokens are resolved via the vault provider (`ZEPH_DISCORD_TOKEN` and `ZEPH_DISCORD_APP_ID` secrets).

### Allowlists

Restrict access by Discord user IDs, role IDs, or channel IDs:

```toml
[discord]
allowed_user_ids = ["123456789012345678"]
allowed_role_ids = ["987654321098765432"]
allowed_channel_ids = ["111222333444555666"]
```

When all allowlists are empty, the bot accepts messages from all users in all channels.

### Slash Commands

Zeph registers two slash commands on startup via the Discord REST API:

| Command | Description |
|---------|-------------|
| `/ask <message>` | Send a message to the agent |
| `/clear` | Reset conversation context |

### Streaming Behavior

Discord enforces a rate limit of 5 message edits per 5 seconds. Streaming uses edit-in-place with a 1.5-second throttle:

- First chunk sends a new message immediately
- Subsequent chunks edit the existing message in-place (throttled to 1.5s intervals)
- On flush, a final edit delivers the complete response
- Long messages (>2000 chars) are automatically split

## Slack Channel

Run Zeph as a Slack bot with Events API webhook, HMAC-SHA256 signature verification, and streaming via message updates. Requires the `slack` feature flag.

```bash
cargo build --release --features slack
```

### Setup

1. Create a Slack app at [api.slack.com/apps](https://api.slack.com/apps).
2. Under **OAuth & Permissions**, add the `chat:write` scope and install to your workspace. Copy the Bot User OAuth Token.
3. Under **Basic Information**, copy the Signing Secret.
4. Under **Event Subscriptions**, enable events and set the Request URL to `http://<host>:<port>/slack/events`.
5. Subscribe to the `message.channels` and `message.im` bot events.

6. Configure the tokens:

   ```bash
   ZEPH_SLACK_BOT_TOKEN="xoxb-..." ZEPH_SLACK_SIGNING_SECRET="..." ./zeph
   ```

   Or in `config/default.toml`:

   ```toml
   [slack]
   bot_token = "xoxb-..."
   signing_secret = "..."
   port = 3000
   webhook_host = "127.0.0.1"
   allowed_user_ids = []
   allowed_channel_ids = []
   ```

> Tokens are resolved via the vault provider (`ZEPH_SLACK_BOT_TOKEN` and `ZEPH_SLACK_SIGNING_SECRET` secrets).

### Allowlists

Restrict access by Slack user IDs or channel IDs:

```toml
[slack]
allowed_user_ids = ["U01ABC123"]
allowed_channel_ids = ["C01XYZ456"]
```

When allowlists are empty, the bot accepts messages from all users in all channels.

### Security

- All incoming webhook requests are verified using HMAC-SHA256 with the signing secret (constant-time comparison)
- Requests with timestamps older than 5 minutes are rejected (replay protection)
- Request body size is limited to 256KB
- The bot filters its own messages to prevent infinite feedback loops (via `auth.test` at startup)

### Streaming Behavior

Slack enforces rate limits on `chat.update`. Streaming uses message updates with a 2-second throttle:

- First chunk posts a new message via `chat.postMessage`
- Subsequent chunks update the message via `chat.update` (throttled to 2s intervals)
- On flush, a final update delivers the complete response

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
2. `[discord]` config with token → Discord channel (requires `discord` feature)
3. `[slack]` config with bot_token → Slack channel (requires `slack` feature)
4. `ZEPH_TELEGRAM_TOKEN` set → Telegram channel
5. Otherwise → CLI channel

Only one channel is active per session.
