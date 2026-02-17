# Tool System

Zeph provides a typed tool system that gives the LLM structured access to file operations, shell commands, and web scraping. Each executor owns its tool definitions with schemas derived from Rust structs via `schemars`, ensuring a single source of truth between deserialization and prompt generation.

## Tool Registry

Each tool executor declares its definitions via `tool_definitions()`. On every LLM turn the agent collects all definitions into a `ToolRegistry` and renders them into the system prompt as a `<tools>` catalog. Tool parameter schemas are auto-generated from Rust structs using `#[derive(JsonSchema)]` from the `schemars` crate.

| Tool ID | Description | Invocation | Required Parameters | Optional Parameters |
|---------|-------------|------------|---------------------|---------------------|
| `bash` | Execute a shell command | ` ```bash ` | `command` (string) | |
| `read` | Read file contents | `ToolCall` | `path` (string) | `offset` (integer), `limit` (integer) |
| `edit` | Replace a string in a file | `ToolCall` | `path` (string), `old_string` (string), `new_string` (string) | |
| `write` | Write content to a file | `ToolCall` | `path` (string), `content` (string) | |
| `glob` | Find files matching a glob pattern | `ToolCall` | `pattern` (string) | |
| `grep` | Search file contents with regex | `ToolCall` | `pattern` (string) | `path` (string), `case_sensitive` (boolean) |
| `web_scrape` | Scrape data from a web page via CSS selectors | ` ```scrape ` | `url` (string), `select` (string) | `extract` (string), `limit` (integer) |

## FileExecutor

`FileExecutor` handles the file-oriented tools (`read`, `write`, `edit`, `glob`, `grep`) in a sandboxed environment. All file paths are validated against an allowlist before any I/O operation.

- If `allowed_paths` is empty, the sandbox defaults to the current working directory.
- Paths are resolved via ancestor-walk canonicalization to prevent traversal attacks on non-existing paths.
- `glob` results are filtered post-match to exclude files outside the sandbox.
- `grep` validates the search directory before scanning.

See [Security](../security.md#file-executor-sandbox) for details on the path validation mechanism.

## Native Tool Use

Providers that support structured tool calling (Claude, OpenAI) use the native API-level tool mechanism instead of text-based fenced blocks. The agent detects this via `LlmProvider::supports_tool_use()` and switches to the native path automatically.

In native mode:

- Tool definitions (name, description, JSON Schema parameters) are passed to the LLM API alongside the messages.
- The LLM returns structured `tool_use` content blocks with typed parameters.
- The agent executes each tool call and sends results back as `tool_result` messages.
- The system prompt instructs the LLM to use the structured mechanism, not fenced code blocks.

The native path uses the same tool executors and permission checks as the legacy path. The only difference is how tools are invoked and results are returned — structured JSON instead of text parsing.

Types involved: `ToolDefinition` (name + description + JSON Schema), `ChatResponse` (Text or ToolUse), `ToolUseRequest` (id + name + input), and `ToolUse`/`ToolResult` variants in `MessagePart`.

Prompt caching is enabled automatically for Anthropic and OpenAI providers, reducing latency and cost when the system prompt and tool definitions remain stable across turns.

## Legacy Text Extraction

Providers without native tool support (Ollama, Candle) use text-based tool invocation, distinguished by `InvocationHint` on each `ToolDef`:

1. **Fenced block** (`InvocationHint::FencedBlock("bash")` / `FencedBlock("scrape")`) — the LLM emits a fenced code block with the specified tag. `ShellExecutor` handles ` ```bash ` blocks, `WebScrapeExecutor` handles ` ```scrape ` blocks containing JSON with CSS selectors.
2. **Structured tool call** (`InvocationHint::ToolCall`) — the LLM emits a `ToolCall` with `tool_id` and typed `params`. `CompositeExecutor` routes the call to `FileExecutor` for file tools.

Both modes coexist in the same iteration. The system prompt includes invocation instructions per tool so the LLM knows exactly which format to use.

## Iteration Control

The agent loop iterates tool execution until the LLM produces a response with no tool invocations, or one of the safety limits is hit.

### Iteration cap

Controlled by `max_tool_iterations` (default: 10). The previous hardcoded limit of 3 is replaced by this configurable value.

```toml
[agent]
max_tool_iterations = 10
```

Environment variable: `ZEPH_AGENT_MAX_TOOL_ITERATIONS`.

### Doom-loop detection

If 3 consecutive tool iterations produce identical output strings, the loop breaks and the agent notifies the user. This prevents infinite loops where the LLM repeatedly issues the same failing command.

### Context budget check

At the start of each iteration, the agent estimates total token usage. If usage exceeds 80% of the configured `context_budget_tokens`, the loop stops to avoid exceeding the model's context window.

## Permissions

The `[tools.permissions]` section defines pattern-based access control per tool. Each tool ID maps to an ordered array of rules. Rules use glob patterns matched case-insensitively against the tool input (command string for `bash`, file path for file tools). First matching rule wins; if no rule matches, the default action is `Ask`.

Three actions are available:

| Action | Behavior |
|--------|----------|
| `allow` | Execute silently without confirmation |
| `ask` | Prompt the user for confirmation before execution |
| `deny` | Block execution; denied tools are hidden from the LLM system prompt |

```toml
[tools.permissions.bash]
[[tools.permissions.bash]]
pattern = "*sudo*"
action = "deny"

[[tools.permissions.bash]]
pattern = "cargo *"
action = "allow"

[[tools.permissions.bash]]
pattern = "*"
action = "ask"
```

When `[tools.permissions]` is absent, legacy `blocked_commands` and `confirm_patterns` from `[tools.shell]` are automatically converted to equivalent permission rules (`deny` and `ask` respectively).

## Output Overflow

Tool output exceeding 30 000 characters is truncated (head + tail split) before being sent to the LLM. The full untruncated output is saved to `~/.zeph/data/tool-output/{uuid}.txt`, and the truncated message includes the file path so the LLM can read the complete output if needed.

Stale overflow files older than 24 hours are cleaned up automatically on startup.

## Output Filter Pipeline

Before tool output reaches the LLM context, it passes through a command-aware filter pipeline that strips noise and reduces token consumption. Filters are matched by command pattern and composed in sequence.

### Built-in Filters

| Filter | Matches | What it removes |
|--------|---------|----------------|
| `TestOutputFilter` | `cargo test`, `cargo nextest`, `pytest`, `go test` | Passing test lines, verbose output; keeps failures and summary |
| `ClippyFilter` | `cargo clippy` | Duplicate diagnostic paths, redundant `help:` lines |
| `GitFilter` | `git log`, `git diff` | Limits log entries (default: 20), diff line count (default: 500) |
| `DirListingFilter` | `ls`, `find`, `tree` | Collapses redundant whitespace and deduplicates paths |
| `LogDedupFilter` | any command with repetitive log output | Deduplicates consecutive identical lines |

All filters also strip ANSI escape sequences, carriage-return progress bars, and collapse consecutive blank lines (`sanitize_output`).

### Security Pass

After filtering, a security scan runs over the **raw** (pre-filter) output. If credential-shaped patterns are found (API keys, tokens, passwords), a warning is appended to the filtered output so the LLM is aware without exposing the value. Additional regex patterns can be configured via `[tools.filters.security] extra_patterns`.

### FilterConfidence

Each filter reports a confidence level:

| Level | Meaning |
|-------|---------|
| `Full` | Filter is certain it handled this output correctly |
| `Partial` | Heuristic match; some content may have been over-filtered |
| `Fallback` | Pattern matched but output structure was unexpected |

When multiple filters compose in a pipeline, the worst confidence across stages is propagated. Confidence distribution is tracked in [TUI filter metrics](tui.md#filter-metrics).

### Inline Filter Stats (CLI)

In CLI mode, after each filtered tool execution a one-line summary is printed to the conversation:

```
[shell] 342 lines -> 28 lines, 91.8% filtered
```

This appears only when lines were actually removed. It lets you verify the filter is working and estimate token savings without opening the TUI.

### Configuration

```toml
[tools.filters]
enabled = true            # Master switch (default: true)

[tools.filters.test]
enabled = true
max_failures = 10         # Max failing tests to show (default: 10)
truncate_stack_trace = 50 # Stack trace line limit (default: 50)

[tools.filters.git]
enabled = true
max_log_entries = 20      # Max git log entries (default: 20)
max_diff_lines = 500      # Max diff lines (default: 500)

[tools.filters.clippy]
enabled = true

[tools.filters.dir_listing]
enabled = true

[tools.filters.log_dedup]
enabled = true

[tools.filters.security]
enabled = true
extra_patterns = []       # Additional regex patterns to flag as credentials
```

Individual filters can be disabled without affecting others.

## Configuration

```toml
[agent]
max_tool_iterations = 10   # Max tool loop iterations (default: 10)

[tools]
enabled = true
summarize_output = false

[tools.shell]
timeout = 30
allowed_paths = []         # Sandbox directories (empty = cwd only)

[tools.file]
allowed_paths = []         # Sandbox directories for file tools (empty = cwd only)

# Pattern-based permissions (optional; overrides legacy blocked_commands/confirm_patterns)
# [tools.permissions.bash]
# [[tools.permissions.bash]]
# pattern = "cargo *"
# action = "allow"
```

The `tools.file.allowed_paths` setting controls which directories `FileExecutor` can access for `read`, `write`, `edit`, `glob`, and `grep` operations. Shell and file sandboxes are configured independently.

| Variable | Description |
|----------|-------------|
| `ZEPH_AGENT_MAX_TOOL_ITERATIONS` | Max tool loop iterations (default: 10) |
