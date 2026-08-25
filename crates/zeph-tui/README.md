# zeph-tui

[![Crates.io](https://img.shields.io/crates/v/zeph-tui)](https://crates.io/crates/zeph-tui)
[![docs.rs](https://img.shields.io/docsrs/zeph-tui)](https://docs.rs/zeph-tui)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

Ratatui-based TUI dashboard with real-time metrics and multi-session support for Zeph.

## Overview

Provides a terminal UI for monitoring the Zeph agent in real time. Built on ratatui and crossterm, it renders live token usage, latency histograms, conversation history, and skill activity. The skills panel includes Wilson score confidence bars showing each skill's posterior reliability estimate. Supports multiple concurrent sessions via `SessionRegistry` — cycle between sessions with `/session next` / `/session prev` (or the `session:next` / `session:prev` palette entries) and close them with `/session close`. Feature-gated behind `tui`.

## Key Modules

- **app** — `App` state machine driving the render/event loop; uses a dirty flag to skip redraws when state is unchanged, reducing idle CPU usage
- **channel** — `TuiChannel` implementing the `Channel` trait for agent I/O
- **command** — `TuiCommand` plus the fuzzy-matched palette registries: `command_registry()` (view / session / app / plugin entries), `daemon_command_registry()` (`daemon:connect`, `daemon:disconnect`, `daemon:status`), and `extra_command_registry()` (infra, agent/plan, graph/experiment, cocoon, clipboard, knowledge entries)
- **event** — `AgentEvent`, `AppEvent`, `EventReader` for async event dispatch
- **file_picker** — `FileIndex`, the background-built, recency-ordered workspace file index (`ignore` crate) that feeds the `@` mention picker; it is a data source only, no longer a UI surface of its own
- **highlight** — syntax highlighting for code blocks
- **hyperlink** — OSC 8 clickable hyperlinks for bare URLs and markdown links
- **layout** — panel arrangement and responsive grid; `fit_panel_heights` is the integer max-min fair water-filling allocator that sizes the four side-panel slots from their `PanelDemand`s (`Collapsed` / `Rows(n)` / `Greedy`), and `AppLayout::compute` takes a `PanelSizing` (per-slot demands + focused slot) rather than a plain visibility flag set
- **metrics** — `MetricsCollector`, `MetricsSnapshot` for live telemetry; skill confidence bars rendered as `[████░░░░] 73% (42 uses)` using Wilson score posterior from the skills registry; filter savings percentage shown in the status bar (e.g. `Filters: 78%`); `SEC` indicator in status bar shows injection flag count when nonzero; compaction probe metrics panel showing pass/soft-fail/fail/error rates; `Backfilling embeddings: N/M (X%)` status bar entry during embed backfill (clears on completion)
- **theme** — color palette and style definitions
- **widgets** — reusable ratatui widget components; includes `mention_picker` (the inline `@` popup, see below); `subagents` widget with a 5-state FSM panel (`List` → `Detail` → `Create` → `Edit` → `ConfirmDelete`) for interactive management of sub-agent definition files; `security` widget renders a side panel with a real-time security event feed (injection flags, exfiltration blocks, quarantine invocations, truncations); `plan_view` widget renders a live task graph table with per-row status spinners, status colors (Running=Yellow, Completed=Green, Failed=Red), and a 30-second stale cleanup — toggled with `p` (requires `orchestration` feature); `memory` widget displays compaction probe metrics (pass/soft-fail/fail/error distribution with percentage bars); `settings` widget renders a read-only, tabbed (Providers / MCP / Agents) view of live configuration sourced from `MetricsSnapshot`, toggled with `S`; `transcript_search` widget implements a Ctrl+F highlight-and-scroll transcript search overlay (mirrors the Ctrl+R reverse-search pattern); `task_registry` widget shows the live `TaskSupervisor` task list, toggled with `t`
- **error** — `TuiError` typed error enum (Io, Channel)

## Interrupt and quit (`Ctrl+C`)

`Ctrl+C` is handled globally, ahead of every overlay and input mode, and its meaning depends on whether the agent is working:

| Agent state | `Ctrl+C` |
|-------------|----------|
| Busy | Cancels the running turn immediately |
| Idle | Arms a double-press quit window — press again to exit |

While a turn is running, the input separator row carries a `ctrl+c to interrupt` hint. `q` (normal mode) and the `app:quit` palette entry remain the direct exit paths; `Esc` no longer quits, and is reserved for dismissing overlays and leaving insert mode.

## Inline `@` mention picker

Typing `@` in the input opens a non-modal popup above the input row. It never steals keystrokes: every character still lands in the input buffer, and the picker derives its query from the buffer, so cursor movement, paste, and backspace all behave normally. On an empty query, results are ordered by recency (uncommitted changes first, then mtime descending) from the background-built `FileIndex`.

Results are grouped into category tabs — `All`, `Files`, `Skills`, `Agents` — capped at 10 entries.

| Key | Action |
|-----|--------|
| `Left` / `Right` | Cycle category tabs (does not move the input cursor while the picker is open) |
| `Up` / `Down` | Move selection |
| `Tab` / `Enter` | Accept the selected mention |
| `Esc` | Close the picker |

Moving the cursor out of the `@query` span closes the popup automatically.

## Side-panel sizing

Side panels are sized from their own content by default. `fit_panel_heights` grants each visible slot at least one identity row, never more rows than it asked for, and leaves any surplus as blank space at the bottom of the column.

```toml
[tui]
panel_sizing = "auto"   # or "even"
```

| Mode | Behavior |
|------|----------|
| `auto` (default) | Each unpinned panel is sized from its `desired_height` via max-min fair water-filling |
| `even` | Unpinned panels split the column evenly regardless of content |

Switch at runtime with `/panel_sizing` (toggles), `/panel_sizing auto`, `/panel_sizing even`, or the `app:panel-sizing` palette entry. Individual panels can still be pinned to a single summary row independently of this setting.

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

The `SubAgents` side panel (`a` keybinding) was extended in v0.18.1 with live status tracking for running sub-agents and an inline transcript viewer.

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

**Live transcript forwarding** — when `agents.forward_transcript = true` (env `ZEPH_AGENTS_FORWARD_TRANSCRIPT`, CLI `--forward-subagent-text`), the runtime subagent detail view splits to show a bounded, auto-scrolling tail of the selected sub-agent's full, untruncated per-turn text/thinking output as it is produced — instead of only the existing 120-char once-per-turn status snippet. The panel falls back to the unchanged list-only layout when nothing has been forwarded yet or the area is too short to usefully split.

> [!NOTE]
> Forwarding is opt-in and defaults to `false` (zero behavior change when disabled). Forwarded content passes through `ContentSanitizer` plus the optional `SecretMaskRegistry`/`PiiFilter` layers before reaching the panel.

## Durable panel

Press `D` (or the `durable` command-palette entry) to open a live view of `zeph-durable` executions — status, name, and progress per row, polled every 5 seconds. The header shows the active AEAD/HMAC `key_id` and, when a key-rotation window is open (`[durable] previous_key_id` set), a passive `rotation window open (previous_key_id = N)` indicator plus a matching low-priority status-bar chip. This is read-only visibility — the panel offers no rotation action; rotation stays a restart-required CLI-only operation (`zeph durable rotate-key`).

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
| `/dump-format <json\|raw\|trace>` | Switch the debug dump format at runtime |

Files are written to `{output_dir}/{unix_timestamp}/` with numbered `request.json`, `response.txt`, and `tool-{name}.txt` files for each LLM call and tool execution.

## Settings view

Press `S` (or the `settings` command-palette entry) to open a read-only, tabbed view of the running session's live configuration, sourced from `MetricsSnapshot`:

| Tab | Shows |
|-----|-------|
| Providers | Configured `[[llm.providers]]` entries (name, type, model) — secret fields are never surfaced, via an explicit whitelist field-copy |
| MCP | Configured MCP servers and their live connection status |
| Agents | Configured sub-agent definitions (templates), not runtime instances |

Write/edit is out of scope for this view — it is read-only by design.

## Transcript search

Press `Ctrl+F` (or the `search:transcript` command-palette entry) to open a case-insensitive substring search overlay over the conversation transcript, mirroring the existing `Ctrl+R` reverse-search interaction: highlight-and-scroll (not filter), cycle matches, `Esc` restores the pre-search scroll position, `Enter` accepts.

## Command palette

The command palette is opened with `:` in normal mode. Type to fuzzy-filter entries, then press Enter to execute.

Entries come from three registries in the `command` module — `command_registry()` (core), `daemon_command_registry()`, and `extra_command_registry()` — which are the authoritative source for the current set.

**Core — views and panels**

| Entry | Description |
|-------|-------------|
| `skill:list` | List all loaded skills |
| `mcp:list` | List MCP servers and registered tools |
| `memory:stats` | Show SQLite message count and vector store status |
| `view:cost` | Show token usage and cost breakdown |
| `view:tools` | List available tools |
| `view:config` | Show active configuration |
| `view:autonomy` | Show autonomy/trust level |
| `view:latency` | Show classifier and turn-latency breakdown |
| `tasks` | Toggle the task registry panel (`t` shortcut), showing live `TaskSupervisor` tasks |
| `fleet` | Show agent sessions (`f` shortcut) |
| `durable` | Show durable executions with key rotation status (`D` shortcut) |
| `settings` | Browse configured providers, MCP servers, and agents (`S` shortcut) |
| `search:transcript` | Find in conversation (`Ctrl+F` shortcut) |
| `integrity:status` | Transcript/session tamper-evidence status |

**Core — sessions and app**

| Entry | Description |
|-------|-------------|
| `session:new` | Start a new conversation session |
| `session:next` / `session:prev` | Cycle to the next / previous open session |
| `session:close` | Close the current session (refused when only one session is open) |
| `session:history` | Browse session history (`H` shortcut) |
| `session:undo` / `session:redo` | Undo / re-apply the last shell checkpoint |
| `app:quit` | Exit the TUI (`q` shortcut) |
| `app:help` | Show keybindings help (`?` shortcut) |
| `app:theme` | Cycle theme (zephyr → zephyr-light → high-contrast) |
| `app:theme-list` | List available themes |
| `app:mouse` | Toggle mouse mode (wheel scroll, click focus) |
| `app:equalizer` | Toggle the compact VU-meter in the busy separator row |
| `app:panel-sizing` | Toggle side-panel sizing between `auto` and `even` |
| `plugin:list` / `plugin:add` / `plugin:remove` / `plugin:overlay` | Manage installed plugins |

**Daemon**

| Entry | Description |
|-------|-------------|
| `daemon:connect` | Attach to a running daemon |
| `daemon:disconnect` | Detach from daemon |
| `daemon:status` | Show daemon connection state |

**Extra — diagnostics, memory, agents, plans**

| Entry | Description |
|-------|-------------|
| `view:filters` | Display output filter hit rates and invocation counts |
| `ingest` | Usage hint for `zeph ingest <path>` |
| `gateway:status` | Show gateway server state — requires `gateway` feature |
| `scheduler:list` | List active scheduled tasks — requires `scheduler` feature |
| `router:stats` | Show Thompson router alpha/beta per provider |
| `security:events` | Show security event history |
| `sandbox:status` | Show sandbox backend, denied domains, fail-if-unavailable |
| `log:status` | Show log file path and recent entries |
| `config:migrate` | Show config migration diff (missing parameters) |
| `compaction:status` | Show server-side compaction status |
| `tafc:status` | Show Think-Augmented Function Calling status |
| `memory:forgetting-sweep` | Run the forgetting sweep once |
| `memory:trajectory` / `memory:tree` | Show trajectory / memory-tree statistics |
| `worktree:list` / `worktree:clean` | List or remove stale git worktrees |
| `agent:list` / `agent:status` / `agent:cancel` / `agent:spawn` | Manage running sub-agents |
| `agents:show` / `agents:create` / `agents:edit` / `agents:delete` | Manage sub-agent definitions |
| `plan:status` | Print current plan progress to chat |
| `plan:confirm` | Confirm and execute the pending plan |
| `plan:cancel` | Cancel the active plan |
| `plan:list` | List recent plans |
| `plan:toggle` | Toggle Plan View in the side panel (`p` shortcut) — requires `orchestration` feature |
| `graph:stats` / `graph:entities` / `graph:facts` / `graph:communities` / `graph:backfill` | Knowledge graph inspection — requires `graph-memory` feature |
| `experiment:start` / `experiment:stop` / `experiment:status` / `experiment:report` / `experiment:best` | Self-experimentation — requires `experiments` feature |
| `guidelines:view` | Show compression guidelines |
| `cocoon:status` / `cocoon:models` | Cocoon sidecar status and model list |
| `clipboard:copy` / `clipboard:copyblock` | Copy the last assistant reply / its last code block |
| `knowledge:status` / `knowledge:rollback` / `knowledge:ingest` | Knowledge ingest ledger operations |
| `lsp:status` | Show LSP context injection status |
| `acp:dirs` / `acp:auth-methods` / `acp:status` / `acp:subagent-spawn` | ACP directory allowlist, auth methods, runtime status, sub-agent spawn |

## Features

| Feature | Description |
|---------|-------------|
| `sqlite` | SQLite backend forwarded to `zeph-memory`/`zeph-core`/`zeph-subagent` (enabled by `default`) |
| `postgres` | PostgreSQL backend forwarded to the same crates |
| `clipboard` | System clipboard integration via `arboard` |
| `cocoon` | Enables cocoon-related command palette entries |
| `profiling` | Emits `tracing` instrumentation spans (e.g. around `run_tui`) |

Feature-gated command-palette and slash entries (e.g. `graph:*`, `experiment:*`, `plan:*`, `scheduler:*`, `gateway:*`, `daemon:*`) are driven by feature flags on the root `zeph` crate, not by this crate directly.

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

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
