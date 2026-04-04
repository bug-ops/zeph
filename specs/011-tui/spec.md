# Spec: TUI Dashboard

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
```

## Key Invariants

- Metrics updated every turn — not only when a specific event fires
- `TuiChannel` never panics on terminal resize — must handle `Event::Resize`
- All background operations show spinner before starting, clear on completion
- Security panel must show current `ContentSanitizer` state (not just error events)
- No blocking I/O on the TUI render thread — all heavy work offloaded to tokio tasks
