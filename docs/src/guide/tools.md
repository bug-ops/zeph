# Tool System

Zeph provides a typed tool system that gives the LLM structured access to file operations, shell commands, and web scraping. The system supports two execution modes: fenced bash block extraction (legacy) and structured tool calls with typed parameters.

## Tool Registry

`ToolRegistry` defines 7 built-in tools that are injected into the system prompt as a `<tools>` catalog so the LLM knows what is available.

| Tool ID | Description | Required Parameters | Optional Parameters |
|---------|-------------|---------------------|---------------------|
| `bash` | Execute a shell command | `command` (string) | |
| `read` | Read file contents | `path` (string) | `offset` (integer), `limit` (integer) |
| `edit` | Replace a string in a file | `path` (string), `old_string` (string), `new_string` (string) | |
| `write` | Write content to a file | `path` (string), `content` (string) | |
| `glob` | Find files matching a glob pattern | `pattern` (string) | |
| `grep` | Search file contents with regex | `pattern` (string) | `path` (string), `case_sensitive` (boolean) |
| `web_scrape` | Scrape data from a web page via CSS selectors | `url` (string) | |

## FileExecutor

`FileExecutor` handles the file-oriented tools (`read`, `write`, `edit`, `glob`, `grep`) in a sandboxed environment. All file paths are validated against an allowlist before any I/O operation.

- If `allowed_paths` is empty, the sandbox defaults to the current working directory.
- Paths are resolved via ancestor-walk canonicalization to prevent traversal attacks on non-existing paths.
- `glob` results are filtered post-match to exclude files outside the sandbox.
- `grep` validates the search directory before scanning.

See [Security](../security.md#file-executor-sandbox) for details on the path validation mechanism.

## Dual-Mode Execution

The agent loop supports two tool invocation modes:

1. **Bash extraction** -- the original mode. The LLM emits fenced ` ```bash ``` ` blocks, and `ShellExecutor` parses and runs them through the safety filter.
2. **Structured tool calls** -- the LLM emits a `ToolCall` with `tool_id` and typed `params`. `CompositeExecutor` routes the call to the appropriate backend (`FileExecutor` for file tools, `ShellExecutor` for `bash`, `WebScrapeExecutor` for `web_scrape`).

Both modes coexist in the same iteration. The agent first checks for structured tool calls, then falls back to bash block extraction.

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
