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
  - "[[068-session-persistence/spec]]"
---

# Spec: TUI Dashboard

> [!info]
> ratatui-based dashboard, spinner rule for all background operations,
> visible status indicators, RenderCache for memory efficiency.

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-tui/src/app/` | `TuiApp`, panel layout, event loop |
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
| Fleet | `f` | Read-only table of all agent sessions (active/completed/unknown); auto-refreshed by `AgentEvent::FleetSnapshot` (#3884) |
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

## Fleet Panel (#3884, #4354, #4363)

`Panel::Fleet` (feature-gated `tui`) shows a live table of agent sessions tracked in the `agent_sessions` DB table.

| Column | Content |
|--------|---------|
| Session ID | Truncated UUID |
| Kind | `cli`, `tui`, `telegram`, `discord`, `slack` |
| Status | `active`, `completed`, `unknown` |
| Channel | Channel identifier |
| Started | Wall-clock start time |
| Duration | Elapsed wall time |

- Toggled with `f`
- Read-only: no user interaction, no j/k navigation
- Refresh driven by `AgentEvent::FleetSnapshot`; a background tokio interval task polls `list_agent_sessions` every `[fleet] refresh_interval_secs` (default 5) and sends the event
- Session lifecycle: `upsert_agent_session` on start, `update_agent_session_status` on normal or error exit; `reconcile_stale_sessions` marks stale active rows as `unknown` on startup (single atomic UPDATE, no TOCTOU race)
- CLI subcommand `zeph agents fleet` prints the same data as a formatted table

### Config

```toml
[fleet]
refresh_interval_secs = 5  # default; serde(default) — no migration needed
```

### Key Invariants

- `reconcile_stale_sessions` runs once at startup before any session is registered — never after
- Fleet panel is read-only; the user cannot kill or restart sessions from the TUI
- `AgentEvent::FleetSnapshot` carries the full snapshot; the panel renders it directly without querying the DB again

---

## Reasoning Token Tracking (#3904, #4354)

The Metrics panel displays reasoning tokens (thinking blocks) separately from prompt and completion tokens.

| Metric | Description |
|--------|-------------|
| `reasoning_tokens` | Cumulative count of tokens in `<thinking>` blocks for the session |

`MetricsSnapshot::reasoning_tokens` is updated after each LLM response that contains thinking-block parts. Displayed in the Metrics panel alongside prompt/completion/cached token counts.

---

## Terminal Title (#4354)

When running in TUI mode, the terminal title is set to `Zeph — <session_id_short>` using ANSI escape sequences. The title is updated once at TUI startup and reset to the previous title on exit.

---

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

## Ctrl+R Prompt History Reverse-Search (#4649, #4657, #4678)

`ReverseSearchState` widget adds in-session prompt history reverse-search accessible via
`Ctrl+R` keybinding from TUI Insert mode.

### Behavior

- Scope: in-memory prompt history for the current session only (no cross-session persistence)
- `Ctrl+R`: enter reverse-search mode; input field shows search query, history filtered live
- `Ctrl+R` again: cycle to the next older match
- `Enter`: confirm match, place in input composer, exit reverse-search mode
- `Esc`: exit reverse-search mode without replacing input

### Key Dispatch Order

Key dispatch in TUI Insert mode checks reverse-search **before** slash-autocomplete (invariant C4).
If the user is in reverse-search mode, all keystrokes route to `ReverseSearchState` — slash
autocomplete does not activate.

### Char-Safe Rendering

The reverse-search render uses `floor_char_boundary()` for truncation to prevent panics on
multibyte UTF-8 input (Cyrillic, CJK characters). NEVER truncate at a byte boundary.

### Key Invariants (Ctrl+R)

- Reverse-search scope is single-session, in-memory only — NEVER persist across restarts or share across slots
- Key dispatch MUST check reverse-search before slash-autocomplete (C4 invariant)
- Render truncation MUST use `floor_char_boundary()` — NEVER byte-index truncation
- `Esc` MUST exit without side-effects — NEVER modify the composer input on dismiss

---

## Session Resume Banner and Bounded History Backfill (spec-068 cross-reference)

`specs/068-session-persistence/spec.md` §13 defines `SessionResumeInfo` and `INV-SP-5`/`INV-SP-6` (resume visibility + non-blocking expansion). TUI-specific wiring:

- **Banner rendering:** a new `AgentEvent::ResumeBanner(SessionResumeInfo)` variant (`crates/zeph-tui/src/event.rs`) is emitted once at startup when `is_resume` is true (spec-068 §13.4). It is rendered as a **persistent** line in the header/status area — not a transient `Status` spinner message — since the user must still see it after the first prompt scrolls by. Exact placement (header vs. a dedicated collapsible line above the input) is an implementation choice, not a spec constraint (spec-068 OQ-I).
- **Bounded backfill:** `/history` (and the rebound `session:history`/`SessionBrowser` command, currently dead — see below) hydrates `SessionSlot::messages` via `App::load_history`, but ONLY with the bounded last-N slice already produced by the shared `TranscriptFormatter` (spec-068 §13.6) — never the full reconstructed history. This keeps `App::load_history`'s synchronous push (and `trim_messages`'s `MAX_TUI_MESSAGES=2000` cap) within the render loop's non-blocking contract (`CLAUDE.md` Async & Background Tasks; spec-039).
- **`/history all` non-blocking requirement (INV-SP-6):** MUST NOT synchronously format and push the entire history on the render thread. Format off-thread under a supervised task (`TaskSupervisor`/`BackgroundSupervisor`) and deliver via `AgentEvent`, or paginate (`/history next`). An explicit "this may take a moment" notice MUST precede the operation.
- **Input-history isolation:** the backfill path MUST NOT feed old messages into `input_history` (the readline up-arrow recall, populated at `state.rs:501-505`) — display-only hydration is a distinct code path or flag from the existing input-history-populating call sites in `App::load_history`.
- **Dead-code reactivation:** `App::load_history` (`crates/zeph-tui/src/app/state.rs:476`) and the `SessionBrowser`/`session:history` command (dispatched at `command.rs:343`, currently swallowed by `_ => continue` in `src/tui_bridge.rs:554`) are the intended wiring targets — do not add a parallel renderer.

### Key Invariants (Resume Banner / History Backfill)

- The resume banner is a persistent render, not a transient spinner message — it MUST remain visible after the first prompt (unlike the Spinner Rule below, which is for in-flight operations)
- `/history` backfill MUST bound to last-N before pushing into `SessionSlot::messages` — never push-all-then-trim
- `/history all` MUST run off-thread or paginated — NEVER a synchronous full-history push on the render loop
- Backfilled display messages MUST NOT be written into `input_history`

---

## Ctrl+C Interrupt & Double-Press Quit Semantics (#6646)

Unifies `Ctrl+C` in the TUI around the REPL pattern (Python/IPython): `Ctrl+C`
interrupts the running operation immediately; a second `Ctrl+C` at an idle prompt
exits. This replaces two accident-prone bindings — single-press `Ctrl+C` quitting
outright, and `Esc` cancelling the in-flight agent turn.

### Behavior

| Context | Key | Behavior |
|---|---|---|
| Agent **busy** | `Ctrl+C` | Cancel the current turn immediately (`Action::CancelAgent`) — no double-press, no window |
| Agent **idle**, 1st press | `Ctrl+C` | Do NOT quit; arm a quit window and show `Press Ctrl+C again to exit` in the status bar |
| Agent **idle**, 2nd press ≤ window | `Ctrl+C` | Quit (`Effect::Quit`) |
| Agent **idle**, press > window later | `Ctrl+C` | Fresh first press — re-arms window + hint, does not quit |
| Normal mode | `Esc` | No-op (falls to `_ => None`) — no longer cancels the agent |
| Insert mode | `Esc` | Unchanged — Insert→Normal toggle |
| Normal mode | `q` / `/quit` / `TuiCommand::Quit` | Unchanged — immediate `Effect::Quit`; double-press applies to `Ctrl+C` only |

### Time source (tick-based, not wall-clock)

The double-press window is measured on the existing animation clock `App::anim_tick()`
(alias of `wave_tick`, advanced once per 100 ms by the `tui_loop` heartbeat), the same
monotonic tick that `ToastQueue::born_tick` and the splash shimmer already use for TTL.
`CTRL_C_DOUBLE_PRESS_TICKS = 5` ≈ 500 ms at 100 ms/tick. This keeps unit tests
deterministic (advance via `advance_wave_tick()` rather than sleeping) and puts no
`Instant`/`SystemTime` on the key path. The threshold is quantized to ~100 ms — an
accepted tradeoff, consistent with all other TUI animation timing.

### State and dispatch

- One top-level `App` field `pending_quit_tick: Option<u64>` (Ctrl+C is global, not
  per-session); captured on the first idle press.
- The global `Ctrl+C` check stays the FIRST branch of `decode_key` (above every modal),
  branching on the pure `&self` read `is_agent_busy()`: busy → `Action::CancelAgent`,
  idle → the new `Action::RequestQuit`. `decode_key` stays `&self`; window arming happens
  only in `reduce` (INV-R1) and emits no effect for the first press (INV-R2).
- Hint visibility is a pure function `quit_hint_active() = !is_agent_busy() &&
  pending_quit_tick within window`. It auto-expires with no extra plumbing: the
  `EventReader` `AppEvent::Tick` cadence marks the frame dirty (`event.rs`), so the status
  bar repaints while idle and drops the hint once the window lapses. `pending_quit_tick` is
  not reset on expiry — the delta check is self-correcting (like `ToastQueue`).

### Key Invariants (Ctrl+C)

- The global `Ctrl+C` branch MUST remain the first check in `decode_key` — modals NEVER swallow `Ctrl+C`; only single-vs-double semantics changed
- Double-press timing MUST use `anim_tick()` — NEVER `Instant::now()`/`SystemTime::now()` on the key/decode path
- Window arming MUST mutate state only in `reduce` (INV-R1); `RequestQuit` MUST perform no I/O (INV-R2, `tui-reducer/spec.md`)
- Agent cancellation MUST stay immediate on a single `Ctrl+C` when busy — the double-press delay applies to quit ONLY, NEVER to cancel
- The quit hint MUST be a transient status-bar segment derived purely from `(pending_quit_tick, anim_tick, is_agent_busy)` — NEVER a separate timer task
- `Esc` retains ONLY its Insert→Normal role — it MUST NEVER cancel an agent turn

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
