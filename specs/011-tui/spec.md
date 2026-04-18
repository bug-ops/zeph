---
aliases:
  - TUI Dashboard
  - TUI Interface
  - ratatui Dashboard
tags:
  - sdd
  - spec
  - tui
  - ui
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[007-channels/spec]]"
  - "[[026-tui-subagent-management/spec]]"
  - "[[030-tui-slash-autocomplete/spec]]"
---

# Spec: TUI Dashboard

> [!info]
> ratatui-based dashboard, spinner rule for all background operations,
> visible status indicators, RenderCache for memory efficiency.

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-tui/src/app.rs` | `TuiApp`, panel layout, event loop |
| `crates/zeph-tui/src/channel.rs` | `TuiChannel`, `Channel` trait impl |
| `crates/zeph-tui/src/metrics.rs` | `MetricsCollector`, watch channel |
| `crates/zeph-tui/src/layout.rs` | Panel split logic |
| `crates/zeph-tui/src/command.rs` | `/command` parsing |
| `crates/zeph-tui/src/event.rs` | crossterm event handling, resize |

---

`crates/zeph-tui/` (feature: `tui`) — ratatui-based terminal UI.

## Architecture

```
TuiApp
├── Layout: split into panels (chat, metrics, status bar, plan view)
├── TuiChannel: implements Channel trait, owns stdin/stdout
├── MetricsCollector: Arc<RwLock<>>, updated via watch channel
├── EventLoop: crossterm events → commands → state updates
└── CommandPalette: /commands parsed from chat input
```

## Panel Layout

| Panel | Key | Content |
|---|---|---|
| Chat | (main) | Conversation history, streaming output |
| Metrics | `m` | Token usage, latency, cost, model |
| Plan View | `p` | DAG task graph, task states |
| Security | `s` | Content sanitizer status, quarantine events |
| SubAgents | `a` | Interactive subagent sidebar with j/k navigation and transcript viewer |
| Task Registry | `/tasks` | Live table of all supervised tasks (see below) |
| Status Bar | always | Current operation spinner + short status text |

Tab cycling order includes SubAgents. See `026-tui-subagent-management/spec.md` for full SubAgents panel spec.

## Spinner Rule (NON-NEGOTIABLE)

**Every background or implicit operation must show a visible spinner with a short status message.**

Examples:
- `Searching memory…`
- `Executing tool: shell`
- `Connecting to MCP server…`
- `Indexing repository…`
- `Loading skills…`

Status messages: short, present continuous tense, no punctuation except `…`.

## TuiChannel Invariants

- `TuiChannel` owns stdin/stdout — **mutually exclusive with ACP stdio transport**
- Enforced at startup: `--tui` + ACP stdio config → startup error
- MCP child process stderr must be suppressed: `McpManager::with_suppress_stderr(true)`
- Streaming output: `send_chunk` appends to current message buffer, `send` finalizes
- Tool events (`send_tool_start`, `send_tool_output`) update the metrics panel, not the chat

## Metrics Pipeline

```
MetricsCollector (Arc<RwLock<>>)
└── updated via tokio watch channel every turn (not only on extraction)
```

- Token usage, latency, cost per turn — updated after every LLM response
- Source labels: which provider/model handled each turn
- Graph metrics (if `graph-memory`): entity count, edge count, community count

## Commands

All `/commands` are parsed from chat input:

```
/exit, /quit       — exit TUI
/clear             — clear conversation
/compact           — force context compaction
/plan <subcommand> — orchestration commands
/graph <subcommand>— graph memory commands
/skills            — list active skills
/models            — list available models
/sec               — show security panel
/tasks             — toggle TaskRegistryWidget (supervised task list)
```

## TaskRegistryWidget

`crates/zeph-tui/src/widgets/task_registry.rs` renders a live table of all tasks registered in `TaskSupervisor`:

| Column | Content |
|--------|---------|
| Spinner | Animated spinner when state is `Running` |
| Name | Task name (`Arc<str>`) |
| Origin | Crate that spawned the task |
| State | `Running`, `Aborted`, `Completed`, `Failed` |
| Uptime | Duration since last restart |
| Restarts | Restart count |

- Toggled via `/tasks` command
- Shows a placeholder row when `TaskSupervisor` is unavailable
- Refreshes at the existing 10 fps render interval — no additional timer
- Calls `supervisor.list_tasks()` each frame to populate the table

## RenderCache

`RenderCache` (`crates/zeph-tui/src/render_cache.rs`) caches wrapped/rendered `Line<'static>` vectors per message, keyed by `RenderCacheKey` (content hash × terminal width × display flags).

- `clear()` replaces the entries `Vec` with a new empty `Vec` — releases all cached memory immediately
- `shift(n)` removes the first `n` entries via `drain(0..n)` — used when old messages scroll out of view; avoids re-indexing the full vector
- NEVER use `clear()` as a substitute for `shift()` when the intent is to evict only leading entries — `clear()` throws away all cached renders including still-visible messages

## Embed Backfill Status

When embed backfill is running at startup (TUI mode only), the status bar shows:

```
Backfilling embeddings: {done}/{total} ({pct}%)
```

This is driven by a `tokio::sync::watch` channel from `spawn_embed_backfill()`. The status clears automatically when the channel signals `None` (completion or timeout). No spinner is used — the fraction display is the progress indicator.

## Log Fallback to Platform Log Directory

When TUI mode is active with no `logging.file` configured and OTLP is disabled, `tracing_init` automatically adds a file appender using `default_log_file_path()`:

- macOS: `~/Library/Application Support/Zeph/logs/zeph.log`

This prevents logs from being silently discarded when stdout/stderr are suppressed by the TUI renderer.

## Audit Log Redirect in TUI Mode

`AuditLogger::from_config` accepts `tui_mode: bool`. When `destination = stdout` and TUI mode is active, audit output is redirected to the configured audit file path with a startup `WARN`. Audit logs are never silently dropped.

## Per-Frame Clone Elimination

`visible_messages()` returns a borrowed reference (`Cow::Borrowed`) instead of cloning the message list. This eliminates ~20,000 `ChatMessage` clones/sec at 2000-message history, reducing idle CPU usage proportional to history depth.

## Multi-Session `SessionRegistry`

Issue #3164. `SessionRegistry` holds per-session state (chat messages, input composer, scroll offset, render cache, paste state) in typed `SessionSlot` structs, keyed by stable `SlotId(u64)`.

Phase-1 (current): always exactly one slot (`SlotId::FIRST`). All per-session fields that were previously on `App` have been relocated to `SessionSlot`. `App` retains shared state that is not session-specific (`queued_count`, `pending_count`, `subagent_sidebar`).

Phase-2 (future): multi-slot rendering and tab bar.

### `/session` Commands

| Command | Action |
|---------|--------|
| `/session next` | Cycle to the next session slot (phase-1: no-op, shows placeholder) |
| `/session prev` | Cycle to the previous session slot |
| `/session close` | Close the current session slot (phase-1: no-op if only one slot) |

These commands are intercepted by the TUI app before forwarding to the agent. They do NOT reach the agent loop.

### `SessionSlot` Fields

`SessionSlot` owns: `messages`, `scroll_offset`, `render_cache`, `input`, `cursor_position`, `input_mode`, `input_history`, `history_index`, `draft_input`, `paste_state`, `view_target`, `transcript_cache`, `pending_transcript`, `show_splash`, `plan_view_active`, `status_label`.

### Key Invariants (SessionRegistry)

- `SlotId` is assigned once and never reused within a process lifetime
- `/session` commands are intercepted in `App` before the agent; the agent never sees them
- `SessionRegistry::bootstrap()` always creates a slot with `SlotId::FIRST` — the registry is never empty after construction
- NEVER store conversational LLM state in `SessionRegistry` — only UI rendering state belongs here

---

## Compact Paste Indicator

Issue #3054. When the user pastes multi-line content into the TUI input:

- The input widget shows a compact single-line indicator: `[Paste: N lines]` instead of the raw pasted text
- The full pasted content is preserved in `PasteState` and used for submission
- In the chat history, pasted multi-line content is rendered as a collapsible block (collapsed by default, toggleable with a key)

### Key Invariants

- Paste indicator must never truncate or lose content — `PasteState` holds the complete original text
- Collapsible paste blocks in chat history use the standard render cache (`RenderCache`) — not a separate code path
- Single-line pastes are NOT shown as a compact indicator — only multi-line pastes (≥2 newlines) trigger the indicator

---

## Key Invariants

- Metrics updated every turn — not only when a specific event fires
- `TuiChannel` never panics on terminal resize — must handle `Event::Resize`
- All background operations show spinner before starting, clear on completion
- Security panel must show current `ContentSanitizer` state (not just error events)
- No blocking I/O on the TUI render thread — all heavy work offloaded to tokio tasks
- `RenderCache::clear()` must release memory — never retain stale entries after `/clear`
- `RenderCache::shift()` must be used (not `clear()`) when only leading messages are evicted
- When `destination = stdout` audit log conflicts with TUI, redirect to file — never drop silently
- When TUI suppresses stderr with no log file configured, use platform log dir — never discard logs
