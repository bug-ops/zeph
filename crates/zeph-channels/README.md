# zeph-channels

Channel implementations for the Zeph agent.

## Overview

Implements I/O channel adapters that connect the agent to different frontends. Ships with a CLI channel, Telegram adapter with streaming support, and optional Discord and Slack adapters. The `AnyChannel` enum provides unified dispatch across all channel variants.

## Key modules

| Module | Description |
|--------|-------------|
| `cli` | `CliChannel` — interactive terminal I/O |
| `telegram` | Telegram adapter via teloxide with streaming; voice/audio message detection and file download |
| `discord` | Discord adapter (optional feature) |
| `slack` | Slack adapter (optional feature); audio file detection and download with Bearer auth |
| `any` | `AnyChannel` — enum dispatch over all channels |
| `markdown` | Markdown rendering helpers |
| `error` | `ChannelError` — unified error type |

**Re-exports:** `AnyChannel`, `CliChannel`, `ChannelError`

## Usage

```toml
[dependencies]
zeph-channels = { path = "../zeph-channels" }
```

## License

MIT
