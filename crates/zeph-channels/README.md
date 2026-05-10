# zeph-channels

[![Crates.io](https://img.shields.io/crates/v/zeph-channels)](https://crates.io/crates/zeph-channels)
[![docs.rs](https://img.shields.io/docsrs/zeph-channels)](https://docs.rs/zeph-channels)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)

Multi-channel I/O adapters (CLI, Telegram, Discord, Slack) for Zeph.

## Overview

Implements I/O channel adapters that connect the agent to different frontends. Ships with a CLI channel, Telegram adapter with streaming support, and optional Discord and Slack adapters. The `AnyChannel` enum provides unified dispatch across all channel variants. All channels implement full feature parity for the `Channel` trait: streaming, attachments, and slash commands work identically regardless of the active frontend.

## Key modules

| Module | Description |
|--------|-------------|
| `cli` | `CliChannel` — interactive terminal I/O with persistent input history (rustyline), prefix search, and `/image` command for vision input |
| `telegram` | Telegram adapter via teloxide with streaming; voice/audio message detection and file download; photo message support for vision input; configurable streaming edit interval (`stream_interval_ms`, default 3000 ms, minimum 500 ms) |
| `telegram::guest` | Guest Mode — transparent local axum HTTP proxy that intercepts `getUpdates` responses and surfaces `guest_message` entries (Bot API 10.0) without a second `getUpdates` connection |
| `telegram::bot_to_bot` | Bot-to-Bot communication — registers via `setManagedBotAccessSettings` on startup; per-chat reply-depth tracking via `BotReplyCounters`; configurable `max_bot_chain_depth` |
| `telegram::api` | `TelegramApiClient` — raw HTTP wrapper for Bot API 10.0 methods unavailable in teloxide 0.17: `answer_guest_query`, `get/set_managed_bot_access_settings`, `delete_message_reaction`, `delete_all_message_reactions` |
| `discord` | Discord adapter (optional feature) |
| `slack` | Slack adapter (optional feature); audio file detection and download with Bearer auth |
| `any` | `AnyChannel` — enum dispatch over all channels |
| `markdown` | Markdown rendering helpers |

**Re-exports:** `AnyChannel`, `CliChannel`

> [!NOTE]
> `ChannelError` is defined in `zeph-core::channel` and used directly by all channel adapters. `zeph-channels` does not re-export it.

## Telegram configuration

Key fields in the `[telegram]` config section:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `stream_interval_ms` | u64 | `3000` | Minimum interval between streaming message edits (minimum 500 ms) |
| `guest_mode` | bool | `false` | Enable Bot API 10.0 Guest Mode — surfaces guest messages via a local proxy |
| `bot_to_bot` | bool | `false` | Enable Bot-to-Bot communication via `setManagedBotAccessSettings` |
| `allowed_bots` | `Vec<String>` | `[]` | Telegram user IDs of bots allowed to interact with this agent |
| `max_bot_chain_depth` | usize | `3` | Max consecutive bot replies before the chain is suppressed |

```toml
[telegram]
stream_interval_ms  = 3000
guest_mode          = false
bot_to_bot          = false
allowed_bots        = []
max_bot_chain_depth = 3
```

> [!NOTE]
> Guest Mode spawns a local axum HTTP proxy on an ephemeral port. Bot API 10.0 is required; ensure your bot account has access to guest message updates.

## Features

| Feature | Description |
|---------|-------------|
| `discord` | Discord WebSocket adapter via tokio-tungstenite |
| `slack` | Slack Events API adapter via axum with HMAC-SHA256 signature verification |

## Installation

```bash
cargo add zeph-channels

# With Discord support
cargo add zeph-channels --features discord

# With Slack support
cargo add zeph-channels --features slack
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
