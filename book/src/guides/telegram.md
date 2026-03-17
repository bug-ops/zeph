# Run via Telegram

Deploy Zeph as a Telegram bot with streaming responses, MarkdownV2 formatting, and user whitelisting.

## Setup

1. Create a bot via [@BotFather](https://t.me/BotFather) — send `/newbot` and copy the token.

2. Configure the token:

   ```bash
   ZEPH_TELEGRAM_TOKEN="123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11" zeph
   ```

   Or store in the age vault:

   ```bash
   zeph vault set ZEPH_TELEGRAM_TOKEN "123456:ABC..."
   zeph --vault age
   ```

3. **Required** — restrict access to specific usernames:

   ```toml
   [telegram]
   allowed_users = ["your_username"]
   ```

   The bot refuses to start without at least one allowed user. Messages from unauthorized users are silently rejected.

## Bot Commands

| Command | Description |
|---------|-------------|
| `/start` | Welcome message |
| `/reset` | Reset conversation context |
| `/skills` | List loaded skills |

## Streaming

Telegram has API rate limits, so streaming works differently from CLI:

- First chunk sends a new message immediately
- Subsequent chunks edit the existing message in-place (throttled to one edit per 10 seconds)
- Long messages (>4096 chars) are automatically split
- MarkdownV2 formatting is applied automatically

## Voice and Image Support

- **Voice notes**: automatically transcribed via STT when `stt` feature is enabled
- **Photos**: forwarded to the LLM for visual reasoning (requires vision-capable model)
- See [Audio & Vision](../advanced/multimodal.md) for backend configuration

## Other Channels

Zeph also supports Discord, Slack, CLI, and TUI. See [Channels](../advanced/channels.md) for the full reference.
