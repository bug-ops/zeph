# zeph-channels

[![Crates.io](https://img.shields.io/crates/v/zeph-channels)](https://crates.io/crates/zeph-channels)
[![docs.rs](https://img.shields.io/docsrs/zeph-channels)](https://docs.rs/zeph-channels)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Multi-channel I/O adapters (CLI, Telegram, Discord, Slack) for Zeph.

## Overview

Implements I/O channel adapters that connect the agent to different frontends. Ships with a CLI channel, Telegram adapter with streaming support, and optional Discord and Slack adapters. The `AnyChannel` enum provides unified dispatch across all channel variants. All channels implement full feature parity for the `Channel` trait: streaming, attachments, and slash commands work identically regardless of the active frontend.

## Key modules

| Module | Description |
|--------|-------------|
| `cli` | `CliChannel` — interactive terminal I/O with persistent input history (rustyline), prefix search, and `/image` command for vision input |
| `telegram` | Telegram adapter via teloxide with streaming; voice/audio message detection and file download; photo message support for vision input |
| `discord` | Discord adapter (optional feature) |
| `slack` | Slack adapter (optional feature); audio file detection and download with Bearer auth |
| `any` | `AnyChannel` — enum dispatch over all channels |
| `markdown` | Markdown rendering helpers |

**Re-exports:** `AnyChannel`, `CliChannel`

> [!NOTE]
> `ChannelError` is defined in `zeph-core::channel` and used directly by all channel adapters. `zeph-channels` does not re-export it.

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
