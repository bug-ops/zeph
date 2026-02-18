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
| Zeph v0.9.8 | Provider: orchestrator | Model: claude-son... |
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
- **Input line**: always visible, supports multiline input via Shift+Enter. Shows `[+N queued]` badge when messages are pending
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
| `e` | Toggle expanded/compact view for tool output and diffs |
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
| `Ctrl+K` | Clear message queue |

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
| Code blocks | Syntax-highlighted via tree-sitter (language-aware coloring) with dimmed language tag |
| `# Heading` | Bold + underlined |
| `- list item` | Green bullet (•) prefix |
| `> blockquote` | Dimmed vertical bar (│) prefix |
| `~~strikethrough~~` | Crossed-out modifier |
| `---` | Horizontal rule (─) |

## Diff View

When the agent uses write or edit tools, the TUI renders file changes as syntax-highlighted diffs directly in the chat panel. Diffs are computed using the `similar` crate (line-level) and displayed with visual indicators:

| Element | Rendering |
|---------|-----------|
| Added lines | Green `+` gutter, green background |
| Removed lines | Red `-` gutter, red background |
| Context lines | No gutter marker, default background |
| Header | File path with `+N -M` change summary |

Syntax highlighting (via tree-sitter) is preserved within diff lines for supported languages (Rust, Python, JavaScript, JSON, TOML, Bash).

### Compact and Expanded Modes

Diffs default to **compact mode**, showing a single-line summary (file path with added/removed line counts). Press `e` to toggle **expanded mode**, which renders the full line-by-line diff with syntax highlighting and colored backgrounds.

The same `e` key toggles between compact and expanded for tool output blocks as well.

## Thinking Blocks

When using Ollama models that emit reasoning traces (DeepSeek, Qwen), the `<think>...</think>` segments are rendered in a darker color (DarkGray) to visually separate model reasoning from the final response. Incomplete thinking blocks during streaming are also shown in the darker style.

## Conversation History

On startup, the TUI loads the latest conversation from SQLite and displays it in the chat panel. This provides continuity across sessions.

## Message Queueing

The TUI input line remains interactive during model inference, allowing you to queue up to 10 messages for sequential processing. This is useful for providing follow-up instructions without waiting for the current response to complete.

### Queue Indicator

When messages are pending, a badge appears in the input area:

```text
You: next message here [+3 queued]_
```

The counter shows how many messages are waiting to be processed. Queued messages are drained automatically after each response completes.

### Message Merging

Consecutive messages submitted within 500ms are automatically merged with newline separators. This reduces context fragmentation when you send rapid-fire instructions.

### Clearing the Queue

Press `Ctrl+K` in Insert mode to discard all queued messages. This is useful if you change your mind about pending instructions.

Alternatively, send the `/clear-queue` command to clear the queue programmatically.

### Queue Limits

The queue holds a maximum of 10 messages. When full, new input is silently dropped until the agent drains the queue by processing pending messages.

## Responsive Layout

The TUI adapts to terminal width:

| Width | Layout |
|-------|--------|
| >= 80 cols | Full layout: chat (70%) + side panels (30%) |
| < 80 cols | Side panels hidden, chat takes full width |

## Live Metrics

The TUI dashboard displays real-time metrics collected from the agent loop via `tokio::sync::watch` channel. The render loop polls the watch receiver before every frame at 250 ms intervals, so the display updates continuously even without user input.

| Panel | Metrics |
|-------|---------|
| **Skills** | Active/total skill count, matched skill names per query |
| **Memory** | SQLite message count, conversation ID, Qdrant status, embeddings generated, summaries count, tool output prunes |
| **Resources** | Prompt/completion/total tokens, API calls, last LLM latency (ms), provider and model name, prompt cache read/write tokens, filter stats |

Metrics are updated at key instrumentation points in the agent loop:
- After each LLM call (api_calls, latency, prompt tokens)
- After streaming completes (completion tokens)
- After skill matching (active skills, total skills)
- After message persistence (sqlite message count)
- After summarization (summaries count)
- After each tool execution with filter applied (filter metrics)

Token counts use a `chars/4` estimation (sufficient for dashboard display).

### Filter Metrics

When the output filter pipeline has processed at least one command, the Resources panel shows:

```
Filter: 8/10 commands (80% hit rate)
Filter saved: 1240 tok (72%)
Confidence: F/6 P/2 B/0
```

| Field | Meaning |
|-------|---------|
| `N/M commands` | Filtered / total commands through the pipeline |
| `hit rate` | Percentage of commands where output was actually reduced |
| `saved tokens` | Cumulative estimated tokens saved (`chars_saved / 4`) |
| `%` | Token savings as a fraction of raw token volume |
| `F/P/B` | Confidence distribution: Full / Partial / Fallback counts (see below) |

The filter section only appears when `filter_applications > 0` — it is hidden when no commands have been filtered.

#### Confidence Levels Explained

Each filter reports how confident it is in the result. The `Confidence: F/1 P/0 B/3` line shows cumulative counts across all filtered commands:

| Level | Abbreviation | When assigned | What it means for the output |
|-------|-------------|---------------|------------------------------|
| **Full** | `F` | Filter recognized the output structure completely (e.g. `cargo test` with standard `test result:` summary) | Output is reliably compressed — no useful information lost |
| **Partial** | `P` | Filter matched the command but output had unexpected sections mixed in (e.g. warnings interleaved with test results) | Most noise removed, but some relevant content may have been stripped — inspect if results look incomplete |
| **Fallback** | `B` | Command pattern matched but output structure was unrecognized (e.g. `cargo audit` matched a cargo-prefix filter but has no dedicated handler) | Output returned unchanged or with minimal sanitization only (ANSI stripping, blank line collapse) |

**Example:** `Confidence: F/1 P/0 B/3` means 1 command was filtered with Full confidence (e.g. `cargo test` — 99% savings) and 3 commands fell through to Fallback (e.g. `cargo audit`, `cargo doc`, `cargo tree` — matched the filter pattern but output was passed through as-is).

When multiple filters compose in a [pipeline](tools.md#output-filter-pipeline), the worst confidence across stages is propagated. A `Full` + `Partial` composition yields `Partial`.

## Deferred Model Warmup

When running with Ollama (or an orchestrator with Ollama sub-providers), model warmup is deferred until after the TUI interface renders. This means:

1. The TUI appears immediately — no blank terminal while the model loads into GPU/CPU memory
2. A status indicator ("warming up model...") appears in the chat panel
3. Warmup runs in the background via a spawned tokio task
4. Once complete, the status updates to "model ready" and the agent loop begins processing

If you send a message before warmup finishes, it is queued and processed automatically once the model is ready.

> **Note:** In non-TUI modes (CLI, Telegram), warmup still runs synchronously before the agent loop starts.

## Architecture

The TUI runs as three concurrent loops:

1. **Crossterm event reader** — dedicated OS thread (`std::thread`), sends key/tick/resize events via mpsc
2. **TUI render loop** — tokio task, draws frames at 10 FPS via `tokio::select!`, polls `watch::Receiver` for latest metrics before each draw
3. **Agent loop** — existing `Agent::run()`, communicates via `TuiChannel` and emits metrics via `watch::Sender`

`TuiChannel` implements the `Channel` trait, so it plugs into the agent with zero changes to the generic signature. `MetricsSnapshot` and `MetricsCollector` live in `zeph-core` to avoid circular dependencies — `zeph-tui` re-exports them.

## Configuration

```toml
[tui]
show_source_labels = true   # Show [user]/[zeph]/[tool] prefixes on messages (default: true)
```

Set `show_source_labels = false` to hide the source label prefixes from chat messages for a cleaner look. Environment variable: `ZEPH_TUI_SHOW_SOURCE_LABELS`.

## Tracing

When TUI is active, tracing output is redirected to `zeph.log` to avoid corrupting the terminal display.

## Docker

Docker images are built without the `tui` feature by default (headless operation). To build a Docker image with TUI support:

```bash
docker build -f docker/Dockerfile.dev --build-arg CARGO_FEATURES=tui -t zeph:tui .
```
