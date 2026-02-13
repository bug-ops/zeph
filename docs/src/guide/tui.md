# TUI Dashboard

Zeph includes an optional ratatui-based Terminal User Interface that replaces the plain CLI with a rich dashboard showing real-time agent metrics, conversation history, and an always-visible input line.

## Enabling

The TUI requires the `tui` feature flag (disabled by default):

```bash
cargo build --release --features tui
```

## Running

```bash
# Via CLI argument
zeph --tui

# Via environment variable
ZEPH_TUI=true zeph
```

## Layout

```text
+-------------------------------------------------------------+
| Zeph v0.9.3 | Provider: orchestrator | Model: claude-son... |
+----------------------------------------+--------------------+
|                                        | Skills (3/15)      |
|                                        | - setup-guide      |
|                                        | - git-workflow     |
|                                        |                    |
| [user] Can you check my code?         +--------------------+
|                                        | Memory             |
| [zeph] Sure, let me look at           | SQLite: 142 msgs   |
|        the code structure...           | Qdrant: connected  |
|                                       ▲+--------------------+
+----------------------------------------+--------------------+
| You: write a rust function for fibon_                       |
+-------------------------------------------------------------+
| [Insert] | Skills: 3 | Tokens: 4.2k | Qdrant: OK | 2m 15s |
+-------------------------------------------------------------+
```

- **Chat panel** (left 70%): bottom-up message feed with full markdown rendering (bold, italic, code blocks, lists, headings), scrollbar with proportional thumb, and scroll indicators (▲/▼). Mouse wheel scrolling supported
- **Side panels** (right 30%): skills, memory, and resources metrics — hidden on terminals < 80 cols
- **Input line**: always visible, supports multiline input via Shift+Enter
- **Status bar**: mode indicator, skill count, token usage, uptime
- **Splash screen**: colored block-letter "ZEPH" banner on startup

## Keybindings

### Normal Mode

| Key | Action |
|-----|--------|
| `i` | Enter Insert mode (focus input) |
| `q` | Quit application |
| `Ctrl+C` | Quit application |
| `Up` / `k` | Scroll chat up |
| `Down` / `j` | Scroll chat down |
| `Page Up/Down` | Scroll chat one page |
| `Home` / `End` | Scroll to top / bottom |
| `Mouse wheel` | Scroll chat up/down (3 lines per tick) |
| `d` | Toggle side panels on/off |
| `Tab` | Cycle side panel focus |

### Insert Mode

| Key | Action |
|-----|--------|
| `Enter` | Submit input to agent |
| `Shift+Enter` | Insert newline (multiline input) |
| `Escape` | Switch to Normal mode |
| `Ctrl+C` | Quit application |
| `Ctrl+U` | Clear input line |

### Confirmation Modal

When a destructive command requires confirmation, a modal overlay appears:

| Key | Action |
|-----|--------|
| `Y` / `Enter` | Confirm action |
| `N` / `Escape` | Cancel action |

All other keys are blocked while the modal is visible.

## Markdown Rendering

Chat messages are rendered with full markdown support via `pulldown-cmark`:

| Element | Rendering |
|---------|-----------|
| `**bold**` | Bold modifier |
| `*italic*` | Italic modifier |
| `` `inline code` `` | Blue text with dark background glow |
| Code blocks | Green text with dimmed language tag |
| `# Heading` | Bold + underlined |
| `- list item` | Green bullet (•) prefix |
| `> blockquote` | Dimmed vertical bar (│) prefix |
| `~~strikethrough~~` | Crossed-out modifier |
| `---` | Horizontal rule (─) |

## Thinking Blocks

When using Ollama models that emit reasoning traces (DeepSeek, Qwen), the `<think>...</think>` segments are rendered in a darker color (DarkGray) to visually separate model reasoning from the final response. Incomplete thinking blocks during streaming are also shown in the darker style.

## Conversation History

On startup, the TUI loads the latest conversation from SQLite and displays it in the chat panel. This provides continuity across sessions.

## Responsive Layout

The TUI adapts to terminal width:

| Width | Layout |
|-------|--------|
| >= 80 cols | Full layout: chat (70%) + side panels (30%) |
| < 80 cols | Side panels hidden, chat takes full width |

## Live Metrics

The TUI dashboard displays real-time metrics collected from the agent loop via `tokio::sync::watch` channel:

| Panel | Metrics |
|-------|---------|
| **Skills** | Active/total skill count, matched skill names per query |
| **Memory** | SQLite message count, conversation ID, Qdrant status, embeddings generated, summaries count |
| **Resources** | Prompt/completion/total tokens, API calls, last LLM latency (ms), provider and model name |

Metrics are updated at key instrumentation points in the agent loop:
- After each LLM call (api_calls, latency, prompt tokens)
- After streaming completes (completion tokens)
- After skill matching (active skills, total skills)
- After message persistence (sqlite message count)
- After summarization (summaries count)

Token counts use a `chars/4` estimation (sufficient for dashboard display).

## Architecture

The TUI runs as three concurrent loops:

1. **Crossterm event reader** — dedicated OS thread (`std::thread`), sends key/tick/resize events via mpsc
2. **TUI render loop** — tokio task, draws frames at 10 FPS via `tokio::select!`, polls `watch::Receiver` for latest metrics before each draw
3. **Agent loop** — existing `Agent::run()`, communicates via `TuiChannel` and emits metrics via `watch::Sender`

`TuiChannel` implements the `Channel` trait, so it plugs into the agent with zero changes to the generic signature. `MetricsSnapshot` and `MetricsCollector` live in `zeph-core` to avoid circular dependencies — `zeph-tui` re-exports them.

## Tracing

When TUI is active, tracing output is redirected to `zeph.log` to avoid corrupting the terminal display.

## Docker

Docker images are built without the `tui` feature by default (headless operation). To build a Docker image with TUI support:

```bash
docker build -f Dockerfile.dev --build-arg CARGO_FEATURES=tui -t zeph:tui .
```
