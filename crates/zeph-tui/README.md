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
- **metrics** — `MetricsCollector`, `MetricsSnapshot` for live telemetry; skill confidence bars rendered as `[████░░░░] 73% (42 uses)` using Wilson score posterior from the skills registry; filter savings percentage shown in the status bar (e.g. `Filters: 78%`); `SEC` indicator in status bar shows injection flag count when nonzero; compaction probe metrics panel showing pass/soft-fail/fail/error rates
- **theme** — color palette and style definitions
- **widgets** — reusable ratatui widget components; includes `subagents` widget with a 5-state FSM panel (`List` → `Detail` → `Create` → `Edit` → `ConfirmDelete`) for interactive management of sub-agent definition files; `security` widget renders a side panel with a real-time security event feed (injection flags, exfiltration blocks, quarantine invocations, truncations); `plan_view` widget renders a live task graph table with per-row status spinners, status colors (Running=Yellow, Completed=Green, Failed=Red), and a 30-second stale cleanup — toggled with `p` (requires `orchestration` feature); `memory` widget displays compaction probe metrics (pass/soft-fail/fail/error distribution with percentage bars)
- **error** — `TuiError` typed error enum (Io, Channel)

## Agents management panel

Press `a` in the TUI to open the interactive agents panel. It provides full CRUD over sub-agent definition files without leaving the terminal UI:

| State | Description |
|-------|-------------|
| List | Scrollable list of all discovered definitions with name, scope, model, and permission mode |
| Detail | Full definition view (tools, skills, system prompt, hooks) |
| Create | Inline form wizard — name, description, model, max turns; validates name regex and required fields before writing |
| Edit | Pre-filled form wizard populated from the existing definition |
| ConfirmDelete | Two-step confirmation for non-project-scoped definitions |

Keybindings: `c` — create, `e` — edit, `d` — delete, Enter — detail view, Esc — go back.

## SubAgents sidebar and transcript viewer

The `SubAgents` side panel (`a` keybinding) was extended in v0.18.0 with live status tracking for running sub-agents and an inline transcript viewer.

When a sub-agent is active, the panel shows a spinner alongside the agent name and its current tool/status line. Completed agents display their final turn count.

**Transcript viewer** — press `j`/`k` to navigate the agent list, then `Enter` to open the full JSONL transcript for the selected agent in a scrollable overlay. The overlay renders each turn with role label, timestamp, and message content. Press `Esc` to dismiss.

| Key | Action |
|-----|--------|
| `a` | Toggle SubAgents sidebar |
| `j` / `k` | Move selection down / up in the agent list |
| `Enter` | Open transcript viewer for selected agent |
| `Esc` | Close transcript viewer or sidebar |

> [!NOTE]
> The transcript viewer reads from the persistent JSONL transcript stored by `zeph-core`. Transcripts are available for both active and completed agents as long as the session file exists. Use `/agent resume <id>` to continue a completed session.

## Graph memory commands

When the `graph-memory` feature is enabled, the TUI provides `/graph` slash commands for inspecting the knowledge graph:

| Command | Description |
|---------|-------------|
| `/graph` | Show entity, edge, and community counts |
| `/graph entities` | List all entities with type and last-seen timestamp |
| `/graph facts <entity>` | Show relationships for a specific entity |
| `/graph communities` | List detected communities |
| `/graph backfill [--limit N]` | Process existing messages through graph extraction |

> [!NOTE]
> These commands require `--features graph-memory` (or `--features full`). The graph must be enabled in config (`[memory.graph] enabled = true`) or via the `--graph-memory` CLI flag.

## Experiment commands

When the `experiments` feature is enabled, the TUI provides `/experiment` slash commands for autonomous self-experimentation:

| Command | Description |
|---------|-------------|
| `/experiment start [N]` | Start an experiment session (optional N = max experiments) |
| `/experiment stop` | Stop the running experiment session |
| `/experiment status` | Show current experiment session status |
| `/experiment report` | Print experiment results summary |
| `/experiment best` | Show the best experiment result |

> [!NOTE]
> These commands require `--features experiments` (or `--features full`). Experiments must be enabled in config (`[experiments] enabled = true`).

## Debug dump

Enable debug dump mid-session without restarting the agent:

| Command | Description |
|---------|-------------|
| `/debug-dump` | Enable debug dump using `debug.output_dir` from config |
| `/debug-dump <PATH>` | Enable debug dump writing to a custom directory |

Files are written to `{output_dir}/{unix_timestamp}/` with numbered `request.json`, `response.txt`, and `tool-{name}.txt` files for each LLM call and tool execution.

## Command palette

The command palette is opened with `:` in normal mode. Type to fuzzy-filter entries, then press Enter to execute.

| Entry | Description |
|-------|-------------|
| `skill:list` | List all loaded skills |
| `mcp:list` | List MCP servers and registered tools |
| `memory:stats` | Show SQLite message count and vector store status |
| `view:cost` | Show token usage and cost breakdown |
| `view:tools` | List available tools |
| `view:config` | Show active configuration |
| `view:autonomy` | Show autonomy/trust level |
| `view:filters` | Display output filter hit rates and invocation counts |
| `graph:stats` | Show graph memory statistics — requires `graph-memory` feature |
| `graph:entities` | List graph entities — requires `graph-memory` feature |
| `graph:facts` | Show entity facts (prompts for entity name) — requires `graph-memory` feature |
| `graph:communities` | List graph communities — requires `graph-memory` feature |
| `graph:backfill` | Backfill graph from existing messages — requires `graph-memory` feature |
| `scheduler:list` | List active scheduled tasks (name, kind, mode, next run) — requires `scheduler` feature |
| `gateway:status` | Show gateway server state — requires `gateway` feature |
| `security:events` | Show security event history |
| `plan:status` | Print current plan progress to chat |
| `plan:confirm` | Confirm and execute the pending plan |
| `plan:cancel` | Cancel the active plan |
| `plan:list` | List recent plans |
| `plan:toggle` | Toggle Plan View in the side panel (`p` shortcut) — requires `orchestration` feature |
| `experiment:start` | Start experiment session — requires `experiments` feature |
| `experiment:stop` | Stop running experiment — requires `experiments` feature |
| `experiment:status` | Show experiment status — requires `experiments` feature |
| `experiment:report` | Show experiment results — requires `experiments` feature |
| `experiment:best` | Show best experiment result — requires `experiments` feature |
| `debug:dump` | Enable debug dump to the configured output directory (equivalent to `/debug-dump`) |
| `ingest` | Usage hint for `zeph ingest <path>` |
| `session:new` | Start a new conversation session |
| `session:history` | Browse session history (`H` shortcut) |
| `daemon:connect` | Attach to a running daemon — requires `daemon` feature |
| `daemon:disconnect` | Detach from daemon |
| `daemon:status` | Show daemon connection state |
| `app:quit` | Exit the TUI (`q` shortcut) |
| `app:help` | Show keybindings help (`?` shortcut) |
| `app:theme` | Toggle dark/light theme |

## Features

| Feature | Description |
|---------|-------------|
| `experiments` | Enables experiment-related TUI commands and widgets |
| `guardrail` | Enables the security panel and SEC status bar indicator |
| `lsp-context` | Enables LSP context injection status display |

## Installation

```bash
cargo add zeph-tui
```

Enabled via the `tui` feature flag on the root `zeph` crate:

```bash
cargo run --features tui -- --tui
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
