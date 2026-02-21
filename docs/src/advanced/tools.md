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

See [Security](../reference/security.md#file-executor-sandbox) for details on the path validation mechanism.

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

### Compound Command Matching

LLMs often generate compound shell expressions like `cd /path && cargo test 2>&1 | tail -80`. Filter matchers automatically extract the last command segment after `&&` or `;` separators and strip trailing pipes and redirections before matching. This means `cd /Users/me/project && cargo clippy --workspace -- -D warnings 2>&1` correctly matches the clippy rules — no special configuration needed.

### Built-in Rules

All 19 built-in rules are implemented in the declarative TOML engine and cover: Cargo test/nextest, Clippy, git status, git diff/log, directory listings, log deduplication, Docker, npm/yarn/pnpm, pip, Make, pytest, Go test, Terraform, kubectl, and Homebrew.

All rules also strip ANSI escape sequences, carriage-return progress bars, and collapse consecutive blank lines (`sanitize_output`).

### Security Pass

After filtering, a security scan runs over the **raw** (pre-filter) output. If credential-shaped patterns are found (API keys, tokens, passwords), a warning is appended to the filtered output so the LLM is aware without exposing the value. Additional regex patterns can be configured via `[tools.filters.security] extra_patterns`.

### FilterConfidence

Each filter reports a confidence level:

| Level | Meaning |
|-------|---------|
| `Full` | Filter is certain it handled this output correctly |
| `Partial` | Heuristic match; some content may have been over-filtered |
| `Fallback` | Pattern matched but output structure was unexpected |

When multiple filters compose in a pipeline, the worst confidence across stages is propagated. Confidence distribution is tracked in the [TUI Resources panel](tui.md#confidence-levels-explained) as `F/P/B` counters.

### Inline Filter Stats (CLI)

In CLI mode, after each filtered tool execution a one-line summary is printed to the conversation:

```
[shell] 342 lines -> 28 lines, 91.8% filtered
```

This appears only when lines were actually removed. It lets you verify the filter is working and estimate token savings without opening the TUI.

### Declarative Filters

All filtering is driven by a declarative TOML engine. Rules are loaded at startup from a `filters.toml` file and compiled into the pipeline.

When no user file is present, Zeph uses 19 embedded built-in rules that cover `cargo test`, `cargo nextest`, `cargo clippy`, `git status`, `git diff`, `git log`, directory listings (`ls`, `find`, `tree`), log deduplication, `docker build`, `npm`/`yarn`/`pnpm install`, `pip install`, `make`, `pytest`, `go test`, `terraform`, `kubectl`, and `brew`.

To override, place a `filters.toml` next to your `config.toml` or set `filters_path`:

```toml
[tools.filters]
filters_path = "/path/to/my/filters.toml"
```

#### Rule format

Each rule has a `name`, a `match` block, and a `strategy` block:

```toml
[[rules]]
name = "docker-build"
match = { prefix = "docker build" }
strategy = { type = "strip_noise", patterns = [
  "^Step \\d+/\\d+ : ",
  "^ ---> [a-f0-9]+$",
  "^Removing intermediate container",
  "^\\s*$",
] }

[[rules]]
name = "make"
match = { prefix = "make" }
strategy = { type = "truncate", max_lines = 80, head = 15, tail = 15 }

[[rules]]
name = "npm-install"
match = { regex = "^(npm|yarn|pnpm)\\s+(install|ci|add)" }
strategy = { type = "strip_noise", patterns = ["^npm warn", "^npm notice"] }
enabled = false  # disable without removing
```

#### Match types

| Field | Description |
|-------|-------------|
| `exact` | Matches the command string exactly |
| `prefix` | Matches if the command starts with the value |
| `regex` | Matches the command against a regex (max 512 chars) |

Exactly one of `exact`, `prefix`, or `regex` must be set.

#### Strategies

Nine strategy types are available:

| Strategy | Description |
|----------|-------------|
| `strip_noise` | Removes lines matching any of the provided regex patterns. `Full` confidence when lines removed, `Fallback` otherwise. |
| `truncate` | Keeps the first `head` lines and last `tail` lines when output exceeds `max_lines`. `Partial` confidence when truncated. Defaults: `head = 20`, `tail = 20`. |
| `keep_matching` | Keeps only lines matching at least one of the provided regex patterns; discards the rest. |
| `strip_annotated` | Strips lines that carry a specific annotation prefix (e.g. `note:`, `help:`). |
| `test_summary` | Parses test runner output (Cargo test/nextest, pytest, Go test); retains failures and the final summary, discards passing lines. |
| `group_by_rule` | Groups diagnostic lines (e.g. Clippy warnings) by lint rule and emits one block per rule. |
| `git_status` | Compact-formats `git status` output; preserves branch, staged, and unstaged sections. |
| `git_diff` | Limits diff output to `max_diff_lines` (default: 500); preserves file headers. |
| `dedup` | Normalises timestamps and UUIDs, then deduplicates consecutive identical lines, annotating repeat counts. |

#### Safety limits

- `filters.toml` files larger than 1 MiB are rejected (falls back to defaults).
- Regex patterns longer than 512 characters are rejected.
- Invalid rules are skipped with a warning; valid rules in the same file still load.

### Configuration

```toml
[tools.filters]
enabled = true            # Master switch (default: true)
filters_path = ""         # Custom filters.toml path (default: config dir)

[tools.filters.security]
enabled = true
extra_patterns = []       # Additional regex patterns to flag as credentials
```

Individual rules can be disabled via `enabled = false` in the rule definition without removing them from the file.

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
