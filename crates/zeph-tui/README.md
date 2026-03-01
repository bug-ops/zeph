# zeph-tui

[![Crates.io](https://img.shields.io/crates/v/zeph-tui)](https://crates.io/crates/zeph-tui)
[![docs.rs](https://img.shields.io/docsrs/zeph-tui)](https://docs.rs/zeph-tui)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Ratatui-based TUI dashboard with real-time metrics for Zeph.

## Overview

Provides a terminal UI for monitoring the Zeph agent in real time. Built on ratatui and crossterm, it renders live token usage, latency histograms, conversation history, and skill activity. The skills panel includes Wilson score confidence bars showing each skill's posterior reliability estimate. Feature-gated behind `tui`.

## Key Modules

- **app** — `App` state machine driving the render/event loop; uses a dirty flag to skip redraws when state is unchanged, reducing idle CPU usage
- **channel** — `TuiChannel` implementing the `Channel` trait for agent I/O
- **command_palette** — fuzzy-matching command palette with daemon commands (`daemon:connect`, `daemon:disconnect`, `daemon:status`), action commands (`app:quit`, `app:help`, `session:new`, `app:theme`), and keybinding hints
- **event** — `AgentEvent`, `AppEvent`, `EventReader` for async event dispatch
- **file_picker** — `@`-triggered fuzzy file search with `nucleo-matcher` and `ignore` crate
- **highlight** — syntax highlighting for code blocks
- **hyperlink** — OSC 8 clickable hyperlinks for bare URLs and markdown links
- **layout** — panel arrangement and responsive grid
- **metrics** — `MetricsCollector`, `MetricsSnapshot` for live telemetry; skill confidence bars rendered as `[████░░░░] 73% (42 uses)` using Wilson score posterior from the skills registry; filter savings percentage shown in the status bar (e.g. `Filters: 78%`)
- **theme** — color palette and style definitions
- **widgets** — reusable ratatui widget components
- **error** — `TuiError` typed error enum (Io, Channel)

## Command palette

The command palette (opened with `/`) includes the following built-in commands:

| Command | Description |
|---------|-------------|
| `/ingest <path>` | Ingest documents into Qdrant for RAG retrieval |
| `/gateway status` | Show gateway server state (on/off, request count) |
| `ViewFilters` | Display output filter hit rates and invocation counts |
| `daemon:connect` | Attach to a running daemon session |
| `daemon:disconnect` | Detach from daemon |
| `daemon:status` | Show daemon connection state |
| `session:new` | Start a new conversation session |
| `app:quit` | Exit the TUI |

## Installation

```bash
cargo add zeph-tui
```

Enabled via the `tui` feature flag on the root `zeph` crate.

## License

MIT
