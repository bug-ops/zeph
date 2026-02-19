# Channels

Zeph supports five I/O channels. Each implements the `Channel` trait and can be selected at runtime.

## Overview

| Channel | Activation | Streaming | Confirmation |
|---------|-----------|-----------|--------------|
| CLI | Default | Token-by-token to stdout | y/N prompt |
| Discord | `ZEPH_DISCORD_TOKEN` (requires `discord` feature) | Edit-in-place every 1.5s | Reply "yes" |
| Slack | `ZEPH_SLACK_BOT_TOKEN` (requires `slack` feature) | `chat.update` every 2s | Reply "yes" |
| Telegram | `ZEPH_TELEGRAM_TOKEN` | Edit-in-place every 10s | Reply "yes" |
| TUI | `--tui` flag (requires `tui` feature) | Real-time in chat panel | Auto-confirm |

## CLI Channel

Default channel. Reads from stdin, writes to stdout with immediate streaming. Persistent input history (rustyline): arrow keys to navigate, prefix search, Emacs keybindings (Ctrl+A/E, Alt+B/F, Ctrl+W). History stored in SQLite across restarts.

## Telegram Channel

See [Run via Telegram](../guides/telegram.md) for the setup guide. User whitelisting required (`allowed_users` must not be empty). MarkdownV2 formatting, voice/image support, 10s streaming throttle, 4096 char message splitting.

## Discord Channel

### Setup

1. Create an application at the [Discord Developer Portal](https://discord.com/developers/applications)
2. Copy the bot token, select `bot` + `applications.commands` scopes
3. Configure:

```bash
ZEPH_DISCORD_TOKEN="..." ZEPH_DISCORD_APP_ID="..." zeph
```

```toml
[discord]
allowed_user_ids = []
allowed_role_ids = []
allowed_channel_ids = []
```

When all allowlists are empty, the bot accepts messages from all users.

### Slash Commands

| Command | Description |
|---------|-------------|
| `/ask <message>` | Send a message to the agent |
| `/clear` | Reset conversation context |

Streaming: 1.5s throttle, messages split at 2000 chars.

## Slack Channel

### Setup

1. Create a Slack app at [api.slack.com/apps](https://api.slack.com/apps)
2. Add `chat:write` scope, install to workspace, copy Bot User OAuth Token
3. Copy Signing Secret from Basic Information
4. Enable Event Subscriptions, set URL to `http://<host>:<port>/slack/events`
5. Subscribe to `message.channels` and `message.im` bot events

```bash
ZEPH_SLACK_BOT_TOKEN="xoxb-..." ZEPH_SLACK_SIGNING_SECRET="..." zeph
```

Security: HMAC-SHA256 signature verification, 5-minute replay protection, 256 KB body limit. Self-message filtering via `auth.test` at startup.

Streaming: 2s throttle via `chat.update`.

## TUI Dashboard

Rich terminal interface based on ratatui. See [TUI Dashboard](tui.md) for full documentation.

```bash
zeph --tui
```

## Channel Selection Priority

1. `--tui` flag or `ZEPH_TUI=true` → TUI
2. Discord config with token → Discord
3. Slack config with bot_token → Slack
4. `ZEPH_TELEGRAM_TOKEN` set → Telegram
5. Default → CLI

Only one channel is active per session.

## Message Queueing

Bounded FIFO queue (max 10 messages) handles input received during model inference. Consecutive messages within 500ms are merged. CLI is blocking (no queue). TUI shows a `[+N queued]` badge; press `Ctrl+K` to clear.

## Attachments

Audio and image attachments are supported on Telegram, Slack, CLI/TUI (via `/image`). See [Audio & Vision](multimodal.md).
