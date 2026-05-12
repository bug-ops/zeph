# Run via Telegram

Deploy Zeph as a Telegram bot with streaming responses, MarkdownV2 formatting, user whitelisting, and support for Guest Mode and Bot-to-Bot communication.

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

## Streaming and Response Updates

Telegram has API rate limits, so streaming works differently from CLI. Zeph batches response chunks and updates them on a configurable interval:

- First chunk sends a new message immediately
- Subsequent chunks accumulate and edit the existing message in-place
- Edit interval is configurable via `stream_interval_ms` (default 3000ms, minimum 500ms)
- Long messages (>4096 chars) are automatically split
- MarkdownV2 formatting is applied automatically

### Configuring Stream Interval

Adjust the streaming update frequency to match your network conditions:

```toml
[telegram]
stream_interval_ms = 3000  # Edit every 3 seconds (default)
# For slower connections, increase the interval:
# stream_interval_ms = 5000  # Edit every 5 seconds
# For faster feedback, decrease it:
# stream_interval_ms = 1000  # Edit every 1 second (minimum 500ms)
```

Lower values provide more responsive feedback but consume more API quota. Higher values reduce API calls but responses appear less fluid. Start with the default and adjust based on your network speed and API rate limit tolerance.

## Guest Mode and Bot-to-Bot Communication

Zeph supports advanced Telegram modes for integration with other bots and guest users.

### Guest Mode

Guest Mode allows Zeph to receive messages from guest users who interact via a unique link without having a Telegram account. The bot acts as a transparent proxy for guest queries:

**Use cases:**
- Allow non-Telegram users to chat with Zeph via a web portal
- Integrate Zeph into public-facing applications
- Avoid requiring users to create Telegram accounts

**Configuration:**

```toml
[telegram]
guest_mode = true
```

When enabled, Zeph spawns a local HTTP proxy that intercepts `getUpdates` responses and extracts guest messages. Guest users see a system prompt annotation indicating their guest context, and responses are accumulated before being sent as a single reply.

### Bot-to-Bot Communication

Bot-to-Bot mode allows Zeph to receive and respond to messages relayed from other Telegram bots. This is useful for cascading bot workflows where one bot routes requests to Zeph for specialized processing.

**Use cases:**
- Route specific request types from a primary bot to Zeph for expert processing
- Build bot pipelines where Zeph acts as a specialist in a workflow
- Avoid API conflicts when multiple bots are active in the same chat

**Configuration:**

```toml
[telegram]
bot_to_bot = true
allowed_bots = ["@specialist_bot", "@analyzer_bot"]
max_bot_chain_depth = 3
```

**Fields:**

| Field | Description |
|-------|-------------|
| `bot_to_bot` | Enable bot-to-bot mode (default: false) |
| `allowed_bots` | List of bot usernames allowed to send messages to this bot |
| `max_bot_chain_depth` | Maximum number of consecutive bot replies before cutting the chain (default: 3) |

When enabled, Zeph registers with Telegram via `setManagedBotAccessSettings` on startup and tracks consecutive bot-to-bot reply depth to prevent circular loops. Messages from unauthorized bots are silently rejected.

## Reaction Moderation Tools

Group admins can remove reactions from messages using two tools. Both require the bot to be a group admin and will gracefully degrade to warnings if the admin check fails.

### `telegram_delete_reaction`

Remove a specific reaction from a message. The reaction field must be a non-empty string of up to 10 characters.

```toml
# Example tool invocation in agent code
[tool.telegram_delete_reaction]
chat_id = "-1001234567890"
message_id = 123
reaction = "👍"
```

### `telegram_delete_all_reactions`

Remove all reactions from a message.

```toml
# Remove all reactions from a message
[tool.telegram_delete_all_reactions]
chat_id = "-1001234567890"
message_id = 123
```

Both tools require:
- Bot to be a member of the group
- Bot to have admin privileges in the group
- Valid chat ID and message ID

## Voice and Image Support

- **Voice notes**: automatically transcribed via STT when `stt` feature is enabled
- **Photos**: forwarded to the LLM for visual reasoning (requires vision-capable model)
- See [Audio & Vision](../advanced/multimodal.md) for backend configuration

## Network Timeouts

All Telegram API client connections are subject to a 30-second timeout. This ensures that slow or unresponsive server connections fail fast rather than blocking indefinitely. If you experience timeout errors, check your network connectivity and Telegram's API status at [Telegram Bot API Changelog](https://core.telegram.org/bots/api-changelog).

## Other Channels

Zeph also supports Discord, Slack, CLI, and TUI. See [Channels](../advanced/channels.md) for the full reference.
