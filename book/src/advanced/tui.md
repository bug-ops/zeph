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

# Connect to a remote daemon (requires tui + a2a features)
zeph --connect http://localhost:3000
```

When using `--connect`, the TUI renders token-by-token streaming from the remote agent via A2A SSE. See [Daemon Mode](../guides/daemon-mode.md) for the full setup guide.

## Layout

```text
+-------------------------------------------------------------+
| Zeph v0.12.0 | Provider: orchestrator | Model: claude-son...|
+----------------------------------------+--------------------+
|                                        | Skills (3/15)      |
|                                        | - setup-guide      |
|                                        | - git-workflow     |
|                                        |                    |
| [user] Can you check my code?         +--------------------+
|                                        | Memory             |
| [zeph] Sure, let me look at            | SQLite: 142 msgs   |
|        the code structure...           | Qdrant: connected  |
|                                       ▲+--------------------+
+----------------------------------------+--------------------+
| You: write a rust function for fibon_                       |
+-------------------------------------------------------------+
| [Insert] | Skills: 3 | Tokens: 4.2k | Qdrant: OK | 2m 15s   |
+-------------------------------------------------------------+
```

- **Chat panel** (left 70%): bottom-up message feed with full markdown rendering (bold, italic, code blocks, lists, headings), scrollbar with proportional thumb, and scroll indicators (▲/▼). Mouse wheel scrolling supported
- **Side panels** (right 30%): skills, memory, resources, and security metrics — hidden on terminals < 80 cols. The security panel replaces the sub-agents panel when recent events exist (see [Security Indicators](#security-indicators))
- **Input line**: always visible, supports multiline input via `Shift+Enter` or `Ctrl+J`, and expands up to 3 visible lines. Shows `[+N queued]` badge when messages are pending
- **Status bar**: mode indicator, skill count, token usage, [security indicators](#security-indicators), uptime
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
| `p` | Toggle Plan View / Sub-agents view in the side panel |
| `Tab` | Cycle side panel focus (includes SubAgents panel) |
| `a` | Focus the SubAgents panel |

### Insert Mode

| Key | Action |
|-----|--------|
| `Enter` | Submit input to agent |
| `Shift+Enter` | Insert newline (multiline input) |
| `Ctrl+J` | Insert newline (multiline input) |
| `/` | Open slash-command autocomplete (when input is empty) |
| `@` | Open file picker (fuzzy file search) |
| `Escape` | Switch to Normal mode |
| `Ctrl+C` | Quit application |
| `Ctrl+U` | Clear input line |
| `Ctrl+K` | Clear message queue |
| `Ctrl+P` | Open command palette |

**Slash Command Examples:**
Typing `/` with an empty input shows these and other available commands:
- `/session next|prev|close` — manage conversations
- `/recap` — generate a summary of the current discussion
- `/skills` — list loaded skills
- `/memory` — show memory statistics

### Slash-Command Autocomplete

Typing `/` on an empty input line opens an inline autocomplete dropdown above the input area. The dropdown shows up to 8 matching commands and filters in real time as you type more characters.

| Key | Action |
|-----|--------|
| Any character | Narrow the command list |
| `Up` / `Down` or `Tab` | Move selection |
| `Enter` | Accept selected command and insert into input |
| `Backspace` | Remove last query character (dismisses when query is empty) |
| `Escape` | Dismiss without inserting |

The autocomplete reuses the same command registry as the command palette (`Ctrl+P`). All 51 slash commands are searchable by prefix or keyword.

### File Picker

Typing `@` in Insert mode opens a fuzzy file search popup above the input area. The picker indexes all project files (respecting `.gitignore`) and filters them in real time as you type.

| Key | Action |
|-----|--------|
| Any character | Filter files by fuzzy match |
| `Up` / `Down` | Navigate the result list |
| `Enter` / `Tab` | Insert selected file path at cursor and close |
| `Backspace` | Remove last query character (dismisses if query is empty) |
| `Escape` | Close picker without inserting |

All other keys are blocked while the picker is visible.

### Command Palette

Press `Ctrl+P` in Insert mode to open the command palette. The palette provides read-only agent management commands for inspecting runtime state without leaving the TUI.

| Key | Action |
|-----|--------|
| Any character | Filter commands by fuzzy match |
| `Up` / `Down` | Navigate the command list |
| `Enter` | Execute selected command |
| `Backspace` | Remove last query character |
| `Escape` | Close palette without executing |

Available commands:

| Command | Description | Shortcut |
|---------|-------------|----------|
| `skill:list` | List loaded skills | |
| `mcp:list` | List MCP servers and tools | |
| `memory:stats` | Show memory statistics | |
| `view:cost` | Show cost breakdown | |
| `view:tools` | List available tools | |
| `view:config` | Show active configuration | |
| `view:autonomy` | Show autonomy/trust level | |
| `session:new` | Start new conversation | |
| `session:switch-next` | Switch to next conversation | |
| `session:switch-prev` | Switch to previous conversation | |
| `session:close` | Close current conversation | |
| `session:recap` | Generate summary of current conversation | |
| `app:quit` | Quit application | `q` |
| `app:help` | Show keybindings help | `?` |
| `app:theme` | Toggle theme (dark/light) | |
| `daemon:connect` | Connect to remote daemon | |
| `daemon:disconnect` | Disconnect from daemon | |
| `daemon:status` | Show connection status | |
| `router:stats` | Show Thompson router alpha/beta per provider | |
| `security:events` | Show security event history | |
| `lsp:status` | Show LSP context injection status (hook state, MCP server connection, injection counts, token budget usage). Requires `lsp-context` feature | |
| `plan:status` | Show current plan progress in chat | |
| `plan:confirm` | Confirm a pending plan and begin execution | |
| `plan:cancel` | Cancel the active plan | |
| `plan:list` | List recent plans from persistence | |
| `plan:toggle` | Toggle Plan View on/off in the side panel | `p` |
| `tasks:list` | Show task registry with live metrics (name, state, uptime, restart count) | |

View commands are read-only. Action commands (`session:new`, `app:quit`, `app:theme`) modify application state. Daemon commands manage the remote connection (see [Daemon Mode](../guides/daemon-mode.md)). The palette supports fuzzy matching on both command IDs and labels.

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
| `[text](url)` | Clickable OSC 8 hyperlink (cyan + underline) |

### Clickable Links

Markdown links (`[text](url)`) are rendered as clickable [OSC 8 hyperlinks](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5fede) in supported terminals. The link display text is styled with the link theme (cyan + underline) and the URL is emitted as an OSC 8 escape sequence so the terminal makes it clickable.

Bare URLs (e.g. `https://github.com/...`) are also detected via regex and rendered as clickable hyperlinks.

Security: only `http://` and `https://` schemes are allowed for markdown link URLs. Other schemes (`javascript:`, `data:`, `file:`) are silently filtered. URLs are sanitized to strip ASCII control characters before terminal output.

## Diff View

When the agent uses write or edit tools, the TUI renders file changes as syntax-highlighted diffs directly in the chat panel. Diffs are computed using the `similar` crate (line-level) and displayed with visual indicators:

| Element | Rendering |
|---------|-----------|
| Added lines | Green `+` gutter, green background |
| Removed lines | Red `-` gutter, red background |
| Context lines | No gutter marker, default background |
| Header | File path with `+N -M` change summary |

Syntax highlighting (via tree-sitter) is preserved within diff lines for supported languages (Rust, Python, JavaScript, JSON, TOML, Bash).

## Tool Output Density

Tool execution output (shell commands, file operations, web searches) can be displayed in three different densities to match your preferences. Control density with the `c` key, and configure default density in your config.

### Compact Density

Shows a single-line summary per tool:

```text
● Ran 3 commands
● Explored 2 files  
● Updated 5 lines
```

Consecutive tool calls of the same type are grouped together. Click to expand individual tool.

### Inline Density (Default)

Balances readability with screen space. Shows:
- Tool name and primary arguments (first 2 lines)
- Abbreviated middle section (ellipsis if >6 lines)
- Last 2 lines of output

```text
● shell: git status
  On branch main
  ...
  modified:   src/main.rs
```

Consecutive tools of the same category are grouped with a count badge:

```text
● Ran 3 commands
  ├─ git status
  ├─ cargo build --release
  └─ cargo test
```

### Block Density

Shows full tool output without truncation:

```text
● shell: cargo test
  running 12 tests
  test result: ok. 12 passed; 0 failed
  ...
  test_compression ... ok
```

### Configuration

Set default density in your config:

```toml
[tui]
tool_density = "inline"      # compact, inline, or block (default: inline)
```

Press `c` during a conversation to cycle through densities without config changes.

### Tool Grouping

Consecutive tool calls with the same category are automatically grouped when `tool_density = "inline"` or `"compact"`. Categories are:

| Category | Tools |
|----------|-------|
| Run | shell, bash, sh |
| Explore | ls, find, file_read, etc. |
| Edit | write_file, edit_file, rename, delete |
| Web | web_scrape, brave_search, etc. |
| MCP | All MCP tools |
| Other | Unrecognized tools |

Groups break on role change (user message or system message), tool kind change, or when a tool is streaming.

## Text Selection and Clipboard

### Native Text Selection

The TUI supports native terminal text selection without the Shift modifier. Select text by:

1. Click and drag to select
2. Use keyboard selection (Shift+Arrow) in compatible terminals
3. Triple-click to select a full line or paragraph

Selected text is automatically copied to the system clipboard when you release the mouse or press `Enter`.

### Clipboard Shortcuts

Copy the last assistant message to your system clipboard:

- **`Ctrl+O`** — Copy last assistant response
- **`/copy`** — Copy command (alternative method)

SSH and tmux users: clipboard data is sent via OSC 52 escape sequences, allowing Zeph to write to your local clipboard even on remote machines.

### SSH and Tmux Detection

When running over SSH (detected via `SSH_TTY`, `SSH_CONNECTION`, `SSH_CLIENT` environment variables), clipboard operations automatically fall back to the OSC 52 protocol. This allows clipboard functionality to work in tmux sessions and SSH connections without needing a local xclip or pbcopy.

### Compact and Expanded Modes for Diffs

Diffs default to **compact mode**, showing a single-line summary (file path with added/removed line counts). Press `e` to toggle **expanded mode**, which renders the full line-by-line diff with syntax highlighting and colored backgrounds.

The same `e` key toggles between compact and expanded for tool output blocks as well.

## Thinking Blocks

When using Ollama models that emit reasoning traces (DeepSeek, Qwen), the `<think>...</think>` segments are rendered in a darker color (DarkGray) to visually separate model reasoning from the final response. Incomplete thinking blocks during streaming are also shown in the darker style.

## Multi-Session Management

Zeph supports multiple independent conversations in a single TUI session. Switch between conversations without losing history or context — each maintains its own message thread, input state, and view position.

### Session Operations

Use the `/session` commands to manage conversations:

| Command | Description |
|---------|-------------|
| `/session next` | Switch to the next conversation in creation order |
| `/session prev` | Switch to the previous conversation |
| `/session close` | Close the current conversation and revert to the most recent active session |
| `/session switch <id>` | Jump to a specific conversation by ID |

These commands are also available in the command palette (`Ctrl+P` → search "session").

### Behavior

- **Session switching**: When you switch conversations, the input line is cleared and the chat panel displays the selected conversation's full message history and scroll position.
- **Single-session mode**: If only one conversation exists, `/session next` and `/session prev` are silent no-ops. `/session close` is refused with a status message.
- **Blocked switches**: Session switches are prevented while a confirmation modal or elicitation (input prompt) is active. Complete any pending dialog before switching.
- **Automatic history**: Every conversation's message history, input drafts, and scroll position are automatically saved to SQLite. Closing and reopening Zeph restores the exact state you left.

## Session Recap

When you return to an existing conversation, Zeph automatically generates a brief recap of the prior discussion before accepting new input. The recap is cached and reused across resume sessions unless the conversation history changes.

### Auto-Recap on Resume

When opening a stored conversation that has a cached digest (summary), Zeph displays a recap in the chat panel before the prompt returns focus to the input line. The recap includes:

- Key topics discussed
- Important decisions or outcomes
- Links to relevant files or tools mentioned

Recap is automatic and requires no configuration — it uses the same [Session Digest](../reference/configuration.md#sessionrecap) settings as on-demand recap.

### On-Demand Recap with `/recap`

At any time during a conversation, send the `/recap` command to generate a fresh summary of the current discussion. This is useful for:

- Reorienting yourself after a long conversation
- Getting a summary before making an important decision
- Explicitly updating the cached digest

### Configuration

Recap behavior is controlled via the `[session.recap]` section in your config:

```toml
[session.recap]
# Generate recap automatically when resuming a conversation (default: true)
on_resume = true

# LLM provider for recap generation; empty uses the primary provider (default: empty)
# recap_provider = "fast"

# Max tokens to spend on the recap summary (default: 500)
max_tokens = 500

# Max messages to include in the recap context (default: 50)
max_input_messages = 50
```

**Tips:**

- Set `recap_provider` to a fast, cheap model (e.g., `gpt-4o-mini`, `qwen3:8b`) to keep recap generation quick and inexpensive.
- Increase `max_tokens` for longer or more complex conversations; decrease it for brevity.
- If auto-recap feels intrusive, set `on_resume = false` and use `/recap` only when you explicitly want a summary.

## Conversation History

On startup, the TUI loads the latest conversation from SQLite and displays it in the chat panel. This provides continuity across sessions. Use multi-session management and recap to navigate between conversations.

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

## File Picker

The `@` file picker provides fast file reference insertion without leaving the input area. It uses `nucleo-matcher` (the same fuzzy engine as the Helix editor) for matching and the `ignore` crate for file discovery.

### How It Works

1. Type `@` in Insert mode — a popup appears above the input area
2. Continue typing to narrow results (e.g., `@main.rs`, `@src/app`)
3. The top 10 matches update on every keystroke
4. Press `Enter` or `Tab` to insert the relative file path at the cursor position
5. Press `Escape` to dismiss without inserting

### File Index

The picker walks the project directory on first use and caches the result for 30 seconds. Subsequent `@` triggers within the TTL reuse the cached index. The index:

- Respects `.gitignore` rules via the `ignore` crate
- Excludes hidden files and directories (dotfiles)
- Caps at 50,000 paths to prevent memory spikes in large repositories

### Fuzzy Matching

Matches are scored against the full relative path, so you can search by directory name, file name, or extension. The query `src/app` matches `crates/zeph-tui/src/app.rs` as well as `src/app/mod.rs`.

## Responsive Layout

The TUI adapts to terminal width:

| Width | Layout |
|-------|--------|
| >= 80 cols | Full layout: chat (70%) + side panels (30%) |
| < 80 cols | Side panels hidden, chat takes full width |

## Live Metrics

The TUI dashboard displays real-time metrics collected from the agent loop via `tokio::sync::watch` channel. The render loop polls the watch receiver before every frame. Frames are only emitted when the dirty flag is set (an event was received since the last draw), so the display does not redraw during idle 250 ms ticks with no activity.

| Panel | Metrics |
|-------|---------|
| **Skills** | Active/total skill count, matched skill names per query |
| **Memory** | SQLite message count, conversation ID, Qdrant status, embeddings generated, summaries count, tool output prunes, embed backfill progress |
| **Resources** | Prompt/completion/total tokens, API calls, last LLM latency (ms), provider and model name, prompt cache read/write tokens, filter stats |
| **Compaction** | Compaction probe verdicts (Pass/SoftFail/HardFail/Error counts), last probe score, subgoal registry state (when orchestration active) |
| **Security** | Sanitizer runs/flags/truncations, quarantine calls/failures, exfiltration blocks (images/URLs/memory), recent event log. Shown in place of sub-agents panel when events are recent (< 60s) |

Metrics are updated at key instrumentation points in the agent loop:
- After each LLM call (api_calls, latency, prompt tokens)
- After streaming completes (completion tokens)
- After skill matching (active skills, total skills)
- After message persistence (sqlite message count)
- After summarization (summaries count)
- After each tool execution with filter applied (filter metrics)
- After content sanitization, quarantine, or exfiltration guard activation (security events)

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

### Embed Backfill Progress

When semantic memory is enabled and unembedded messages exist from previous sessions, a background backfill task processes them in micro-batches (32 messages, concurrency 4). The Memory panel shows progress during the backfill:

```
Backfilling embeddings: 128/512 (25%)
```

The progress indicator disappears once all messages have been embedded. Backfill uses bounded memory — only one micro-batch is held in memory at a time — so it does not spike memory usage regardless of how many messages need processing.

#### Confidence Levels Explained

Each filter reports how confident it is in the result. The `Confidence: F/1 P/0 B/3` line shows cumulative counts across all filtered commands:

| Level | Abbreviation | When assigned | What it means for the output |
|-------|-------------|---------------|------------------------------|
| **Full** | `F` | Filter recognized the output structure completely (e.g. `cargo test` with standard `test result:` summary) | Output is reliably compressed — no useful information lost |
| **Partial** | `P` | Filter matched the command but output had unexpected sections mixed in (e.g. warnings interleaved with test results) | Most noise removed, but some relevant content may have been stripped — inspect if results look incomplete |
| **Fallback** | `B` | Command pattern matched but output structure was unrecognized (e.g. `cargo audit` matched a cargo-prefix filter but has no dedicated handler) | Output returned unchanged or with minimal sanitization only (ANSI stripping, blank line collapse) |

**Example:** `Confidence: F/1 P/0 B/3` means 1 command was filtered with Full confidence (e.g. `cargo test` — 99% savings) and 3 commands fell through to Fallback (e.g. `cargo audit`, `cargo doc`, `cargo tree` — matched the filter pattern but output was passed through as-is).

When multiple filters compose in a [pipeline](tools.md#output-filter-pipeline), the worst confidence across stages is propagated. A `Full` + `Partial` composition yields `Partial`.

## Security Indicators

The TUI surfaces the [untrusted content isolation](../reference/security/untrusted-content-isolation.md) pipeline activity through three integration points: a status bar badge, a dedicated side panel, and a command palette entry.

### Status Bar SEC Badge

When the content isolation pipeline detects injection patterns or blocks exfiltration attempts, a `SEC` badge appears in the status bar:

```text
[Insert] | Skills: 3 | Tokens: 4.2k | SEC: 2 flags 1 blocked | API: 12 | 5m 30s
```

| Indicator | Color | Meaning |
|-----------|-------|---------|
| `SEC: N flags` | Yellow | Number of injection patterns detected by the sanitizer |
| `N blocked` | Red | Sum of exfiltration blocks (markdown images stripped + suspicious tool URLs flagged + memory writes guarded) |

The badge is hidden when all security counters are zero.

### Security Side Panel

When security events occur within the last 60 seconds, the bottom-right side panel switches from the sub-agents view to a security view. The panel shows all eight security counters and the five most recent events:

```text
+--------------------+
| Security           |
| Sanitizer runs:  14|
| Inj flags:        3|
| Truncations:      1|
| Quarantine calls:  0|
| Quarantine fails:  0|
| Exfil images:      1|
| Exfil URLs:        0|
| Memory guards:     0|
| Recent events:     |
| 14:32 [inj]  web.. |
|   Detected pattern |
| 14:33 [exfil] llm..|
|   1 image blocked  |
+--------------------+
```

Event categories use color coding:

| Badge | Color | Category |
|-------|-------|----------|
| `[inj]` | Yellow | Injection pattern detected |
| `[exfil]` | Red | Exfiltration attempt blocked |
| `[quar]` | Cyan | Content quarantined |
| `[trunc]` | Dimmed | Content truncated to size limit |

Each event line shows the local time (HH:MM), the category badge, and the source (e.g., `web_scrape`, `mcp_response`, `llm_output`). A second line shows the event detail.

When no events have occurred in the last 60 seconds, the panel reverts to the sub-agents view. When all counters are zero and no events exist, the panel displays "No security events."

### Security Event History

Use the `security:events` command palette entry (`Ctrl+P` then type "security") to print the full event history to the chat panel. The output includes every event in the ring buffer (up to 100 entries) with its category, source, timestamp, and detail. This is useful for reviewing events that have scrolled out of the side panel's 5-event window or that occurred more than 60 seconds ago.

### Event Ring Buffer

Security events are stored in a FIFO ring buffer (capacity 100) within `MetricsSnapshot`. When the buffer is full, the oldest event is evicted. Each event records:

| Field | Constraints |
|-------|-------------|
| `timestamp` | Unix seconds (UTC) |
| `category` | `InjectionFlag`, `ExfiltrationBlock`, `Quarantine`, or `Truncation` |
| `source` | Originating subsystem, capped at 64 characters |
| `detail` | Human-readable description, capped at 128 characters |

Events are emitted by the sanitizer, quarantine, and exfiltration guard subsystems during the agent loop and flow to the TUI via the metrics watch channel.

## Plan View

The TUI shows live plan progress in the side panel.

### Activating Plan View

Press `p` in Normal mode (or use `plan:toggle` from the command palette) to switch the right side panel between the Sub-agents view and the Plan View. The panel switches automatically when a new plan becomes active.

```text
+--------------------+
| Plan: deploy stag… |  ← goal (truncated with …)
| ↻ Preparing env    |  Running  agent-1   12s
| ✓ Build image      |  Done     agent-2   45s
| ✗ Push artifact    |  Failed   agent-2   8s   image push timeout
| · Run smoke tests  |  Pending  —         —
+--------------------+
```

### Status Colors

| Color | Status | Meaning |
|-------|--------|---------|
| Yellow (spinner ↻) | Running | Task is currently executing |
| Green ✓ | Completed | Task finished successfully |
| Red ✗ | Failed | Task failed; error shown in last column |
| White · | Pending | Waiting for dependencies |
| Gray | Skipped / Cancelled | Not executed |

### Panel Header

The panel title shows the plan goal (truncated to fit the panel width with `…`). A spinner appears in the title when at least one task is in Running status:

```
| Plan: build and deploy… [↻] |
```

When no plan is active, the panel shows:

```
| No active plan              |
```

### Plan Commands in TUI

All `/plan` commands work in TUI mode via the input line. The command palette (`Ctrl+P`) provides quick access without typing the full command:

| Command | Palette entry | Description |
|---------|---------------|-------------|
| `/plan <goal>` | — | Decompose goal and queue for confirmation |
| `/plan confirm` | `plan:confirm` | Start execution of the pending plan |
| `/plan cancel` | `plan:cancel` | Cancel the active plan |
| `/plan status` | `plan:status` | Print plan progress to the chat panel |
| `/plan list` | `plan:list` | List recent plans |

### Stale Plan Cleanup

After a plan reaches a terminal state (completed, failed, or cancelled), the Plan View remains visible for 30 seconds so you can review the final status. After 30 seconds the panel automatically reverts to the Sub-agents view. Press `p` at any time to dismiss it earlier or bring it back.

### Requirements

Plan View requires the `tui` feature flag:

```bash
cargo build --release --features tui
```

## SubAgent Sidebar

When [sub-agent orchestration](sub-agents.md) is active, the SubAgents panel in the right sidebar shows each running sub-agent, its current status, and allows you to inspect the full execution transcript.

### Keybindings

| Key | Action |
|-----|--------|
| `a` (Normal mode) | Focus the SubAgents panel |
| `j` / `Down` | Move selection down the agent list |
| `k` / `Up` | Move selection up the agent list |
| `Enter` | Load the JSONL transcript for the selected sub-agent |
| `Esc` | Return focus to the chat panel |
| `Tab` | Cycle side panel focus (SubAgents is included in the rotation) |

### Transcript Viewer

Pressing `Enter` on a sub-agent entry loads its JSONL execution transcript into the chat panel. The transcript shows all messages exchanged by that sub-agent, including tool calls and intermediate reasoning, rendered with the same markdown and diff highlighting as the main conversation. Press `Esc` to return to the normal view.

The SubAgents panel is replaced by the Security panel when recent security events exist (< 60 seconds). Press `a` explicitly to bring the SubAgents panel back when security events are active.

## Deferred Model Warmup

When running with Ollama (or an orchestrator with Ollama sub-providers), model warmup is deferred until after the TUI interface renders. This means:

1. The TUI appears immediately — no blank terminal while the model loads into GPU/CPU memory
2. A status indicator ("warming up model...") appears in the chat panel
3. Warmup runs in the background via a spawned tokio task
4. Once complete, the status updates to "model ready" and the agent loop begins processing

If you send a message before warmup finishes, it is queued and processed automatically once the model is ready.

> **Note:** In non-TUI modes (CLI, Telegram), warmup still runs synchronously before the agent loop starts.

## Performance

### Dirty-Flag Idle Suppression

The render loop tracks a dirty flag that is set whenever a terminal event or agent event is received. Frames are only redrawn when the flag is set — idle 250 ms ticks with no new input or agent activity are skipped entirely. This eliminates redundant redraws during periods of inactivity and reduces idle CPU usage.

### Event Loop Batching

The TUI render loop uses `biased` `tokio::select!` to guarantee input events are always processed before agent events. This prevents keyboard input from being starved during fast LLM streaming or parallel tool execution.

Agent events (streaming chunks, tool output, status updates) are drained in a `try_recv` loop, batching all pending events into a single frame update. This avoids the pathological case where each streaming token triggers a separate redraw.

### Render Cache

Syntax highlighting (tree-sitter) and markdown parsing (pulldown-cmark) results are cached per message. The cache key is a content hash, so only messages whose content actually changed are re-rendered. Cache entries are invalidated on:

- Content change (new streaming chunk appended)
- Terminal resize
- View mode toggle (compact/expanded)

This eliminates redundant parsing work that previously re-processed every visible message on every frame.

`RenderCache::clear()` releases the backing `Vec` allocation (not just clearing entries), preventing memory accumulation across long sessions. `RenderCache::shift(count)` efficiently removes the oldest entries when messages are trimmed during compaction, avoiding a full re-render.

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

## Testing

The TUI has a dedicated test automation infrastructure covering widget snapshots, integration tests with mock event sources, property-based layout fuzzing, and E2E terminal tests. See [TUI Testing](../development/tui-testing.md) for details.
