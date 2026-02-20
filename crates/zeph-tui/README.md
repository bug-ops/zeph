# zeph-tui

ratatui-based TUI dashboard with real-time agent metrics.

## Overview

Provides a terminal UI for monitoring the Zeph agent in real time. Built on ratatui and crossterm, it renders live token usage, latency histograms, conversation history, and skill activity. Feature-gated behind `tui`.

## Key Modules

- **app** — `App` state machine driving the render/event loop
- **channel** — `TuiChannel` implementing the `Channel` trait for agent I/O
- **command_palette** — fuzzy-matching command palette with daemon commands (`daemon:connect`, `daemon:disconnect`, `daemon:status`), action commands (`app:quit`, `app:help`, `session:new`, `app:theme`), and keybinding hints
- **event** — `AgentEvent`, `AppEvent`, `EventReader` for async event dispatch
- **file_picker** — `@`-triggered fuzzy file search with `nucleo-matcher` and `ignore` crate
- **highlight** — syntax highlighting for code blocks
- **hyperlink** — OSC 8 clickable hyperlinks for bare URLs and markdown links
- **layout** — panel arrangement and responsive grid
- **metrics** — `MetricsCollector`, `MetricsSnapshot` for live telemetry
- **theme** — color palette and style definitions
- **widgets** — reusable ratatui widget components
- **error** — `TuiError` typed error enum (Io, Channel)

## Usage

```toml
# Cargo.toml (workspace root)
zeph-tui = { path = "crates/zeph-tui" }
```

Enabled via the `tui` feature flag on the root `zeph` crate.

## License

MIT
