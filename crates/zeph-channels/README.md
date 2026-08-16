# zeph-channels

[![Crates.io](https://img.shields.io/crates/v/zeph-channels)](https://crates.io/crates/zeph-channels)
[![docs.rs](https://img.shields.io/docsrs/zeph-channels)](https://docs.rs/zeph-channels)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Multi-channel I/O adapters (CLI, Telegram, Discord, Slack) for Zeph.

## Overview

Implements I/O channel adapters that connect the agent to different frontends. Ships with a CLI channel, Telegram adapter with streaming support, and optional Discord and Slack adapters. The `AnyChannel` enum provides unified dispatch across all channel variants. All channels implement full feature parity for the `Channel` trait: streaming, attachments, and slash commands work identically regardless of the active frontend.

## Key modules

| Module | Description |
|--------|-------------|
| `cli` | `CliChannel` — interactive terminal I/O with persistent input history (rustyline), prefix search, and `/image` command for vision input |
| `json_cli` | `JsonCliChannel` — active under `--json`; emits JSONL events to stdout and reads prompts from stdin for programmatic/embedding use (logs forced to stderr) |
| `telegram` | Telegram adapter via teloxide 0.17 with streaming; voice/audio message detection and file download; photo message support for vision input; configurable streaming edit interval (`stream_interval_ms`, default 3000 ms, minimum 500 ms); send/edit paths retry on HTTP 429 with backoff (mirroring Discord/Slack). Also hosts Guest Mode — a transparent local axum HTTP proxy that intercepts `getUpdates` responses and surfaces `guest_message` entries (Bot API 10.0) without a second `getUpdates` connection — and Bot-to-Bot support with per-chat reply-depth tracking |
| `telegram_api_ext` | `TelegramApiClient` — raw HTTP wrapper for Bot API 10.0 methods unavailable in teloxide 0.17: `answer_guest_query`, `get`/`set_managed_bot_access_settings`, `delete_message_reaction`, `delete_all_message_reactions` |
| `telegram_moderation` | Telegram-side moderation helpers |
| `discord` | Discord adapter (optional feature) |
| `slack` | Slack adapter (optional feature); audio file detection and download with Bearer auth |
| `streaming` | Shared chunking/flush logic used by the streaming-capable adapters |
| `confirm` | Interactive tool-confirmation prompt shared across channels |
| `auth` | Per-channel sender authorization |
| `any` | `AnyChannel` — enum dispatch over all channels |
| `markdown` | `markdown_to_telegram` renders CommonMark to Telegram `MarkdownV2` (Bot API 10.1 rich text); multi-line blockquotes prefix every line with `>`, nested quotes flatten to a single level (MarkdownV2 has no nested-quote grammar), and long quotes render as Bot API 10.1 expandable (collapsed-by-default) blockquotes at or above `expandable_blockquote_min_lines` |

**Re-exports:** `AnyChannel`, `CliChannel`, `JsonCliChannel`

> [!NOTE]
> `ChannelError` is defined in `zeph-core::channel` and used directly by all channel adapters. `zeph-channels` does not re-export it.

## Telegram configuration

Key fields in the `[telegram]` config section:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `stream_interval_ms` | u64 | `3000` | Minimum interval between streaming message edits (minimum 500 ms) |
| `expandable_blockquote_min_lines` | u32 | `10` | Blockquotes with at least this many lines render as Bot API 10.1 expandable (collapsed-by-default) quotes; `0` disables the expandable form |
| `guest_mode` | bool | `false` | Enable Bot API 10.0 Guest Mode — surfaces guest messages via a local proxy |
| `bot_to_bot` | bool | `false` | Enable Bot-to-Bot communication via `setManagedBotAccessSettings` |
| `allowed_bots` | `Vec<String>` | `[]` | Bot usernames (with the `@` prefix, e.g. `"@my_bot"`) allowed to interact when `bot_to_bot = true`. **Empty means all bots are allowed**, not none |
| `max_bot_chain_depth` | u32 | `1` | Max reply chain depth before Zeph stops responding to bot messages |

```toml
[telegram]
stream_interval_ms              = 3000
expandable_blockquote_min_lines = 10
guest_mode                      = false
bot_to_bot                      = false
allowed_bots                    = ["@my_bot"]
max_bot_chain_depth             = 1
```

> [!NOTE]
> Telegram payloads expose only one level of `reply_to_message` nesting, so
> `max_bot_chain_depth` values above `1` add nothing to structural depth checking. The
> consecutive-reply counter provides the secondary loop prevention across multiple top-level
> exchanges.

> [!NOTE]
> Guest Mode spawns a local axum HTTP proxy on an ephemeral port. Bot API 10.0 is required; ensure your bot account has access to guest message updates.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend (via `zeph-core`, `zeph-tools`) — one backend must be selected for the crate to compile |
| `postgres` | no | PostgreSQL backend |
| `discord` | no | Discord WebSocket adapter via tokio-tungstenite |
| `slack` | no | Slack Events API adapter via axum with HMAC-SHA256 signature verification |
| `profiling` | no | Extra `tracing` instrumentation spans |

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

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
