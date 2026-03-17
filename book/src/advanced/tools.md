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
| `find_path` | Find files matching a glob pattern | `ToolCall` | `path` (string), `pattern` (string) | |
| `list_directory` | List directory entries with type labels | `ToolCall` | `path` (string) | |
| `create_directory` | Create a directory (including parents) | `ToolCall` | `path` (string) | |
| `delete_path` | Delete a file or directory recursively | `ToolCall` | `path` (string) | |
| `move_path` | Move or rename a file or directory | `ToolCall` | `source` (string), `destination` (string) | |
| `copy_path` | Copy a file or directory | `ToolCall` | `source` (string), `destination` (string) | |
| `grep` | Search file contents with regex | `ToolCall` | `pattern` (string) | `path` (string), `case_sensitive` (boolean) |
| `web_scrape` | Scrape data from a web page via CSS selectors | ` ```scrape ` | `url` (string), `select` (string) | `extract` (string), `limit` (integer) |
| `fetch` | Fetch a URL and return plain text (no selector required) | `ToolCall` | `url` (string) | |
| `diagnostics` | Run `cargo check` or `cargo clippy` and return structured diagnostics | `ToolCall` | | `kind` (`check`\|`clippy`), `max_diagnostics` (integer) |

## FileExecutor

`FileExecutor` handles file-oriented tools in a sandboxed environment. All file paths are validated against an allowlist before any I/O operation.

**Read/write tools:** `read`, `write`, `edit`, `grep`

**Navigation tools:** `find_path` (renamed from `glob`), `list_directory`

**Mutation tools:** `create_directory`, `delete_path`, `move_path`, `copy_path`

- If `allowed_paths` is empty, the sandbox defaults to the current working directory.
- Paths are resolved via ancestor-walk canonicalization to prevent traversal attacks on non-existing paths.
- `find_path` results are filtered post-match to exclude entries outside the sandbox.
- `list_directory` uses `symlink_metadata` (lstat) to classify entries as `[dir]`, `[file]`, or `[symlink]` without following symlinks.
- `copy_path` uses lstat when recursing directories to prevent symlink escape via a symlink inside the allowed paths tree.
- `delete_path` guards against recursive deletion of the sandbox root or a path above it.

See [Security](../reference/security.md#file-executor-sandbox) for details on the path validation mechanism.

## WebScrapeExecutor — `fetch` tool

In addition to `web_scrape` (CSS-selector-based extraction), `WebScrapeExecutor` exposes a `fetch` tool that returns plain text from a URL without requiring a selector. SSRF validation (HTTPS-only, private IP block, redirect re-validation) is applied identically to both tools.

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | Yes | HTTPS URL to fetch |

## DiagnosticsExecutor

`DiagnosticsExecutor` runs `cargo check` or `cargo clippy --message-format=json` in the project directory and returns a structured list of diagnostics. Each diagnostic includes:

| Field | Description |
|-------|-------------|
| `severity` | `error` or `warning` |
| `message` | Human-readable description |
| `file` | Source file path |
| `line` | Line number |
| `col` | Column number |

Output is capped at `max_diagnostics` (default: 50) to avoid overwhelming the context. If `cargo` is absent, the tool returns an empty list with a warning rather than panicking.

```toml
[tools.diagnostics]
max_diagnostics = 50   # Maximum number of diagnostics returned (default: 50)
```

> [!TIP]
> Use `kind = "clippy"` for lint warnings in addition to compilation errors. The `check` kind is faster and sufficient for build errors only.

## WebScrapeExecutor

`WebScrapeExecutor` handles the `web_scrape` tool. It fetches an HTTPS URL, parses the HTML response with `scrape-core`, and returns elements matching a CSS selector.

### SSRF Defense Layers

Three defense layers run for every request, including each hop in a redirect chain:

1. **URL validation** — only `https://` is accepted; private hostnames, RFC 1918 IP literals, loopback, link-local, unique-local, IPv4-mapped IPv6, and non-HTTPS schemes are rejected before any socket is opened.
2. **DNS rebinding prevention** — `resolve_and_validate` resolves the hostname and checks every returned IP against the same private-range rules. The validated socket addresses are pinned to the HTTP client via `resolve_to_addrs`, closing the TOCTOU window.
3. **Manual redirect following** — auto-redirect is disabled. Up to 3 redirects are followed manually; each `Location` header value goes through steps 1 and 2 before the next connection is made. This blocks "open redirect to internal service" attacks.

Exceeding 3 hops, or any redirect targeting a blocked host or IP, terminates the request with an error. See [SSRF Protection for Web Scraping](../reference/security.md#ssrf-protection-for-web-scraping) for the full rule set.

### Configuration

```toml
[tools.scrape]
timeout = 15              # Request timeout in seconds (default: 15)
max_body_bytes = 1048576  # Maximum response body size in bytes (default: 1 MiB)
```

### Invocation

```json
{
  "url": "https://example.com",
  "select": "h1",
  "extract": "text",
  "limit": 5
}
```

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `url` | Yes | — | HTTPS URL to fetch |
| `select` | Yes | — | CSS selector |
| `extract` | No | `text` | Extraction mode: `text`, `html`, or `attr:<name>` |
| `limit` | No | `10` | Maximum number of matching elements to return |

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

## Ollama Native Tool Calling

Ollama can use the native tool calling path by setting `tool_use = true` in the `[llm.ollama]` config section:

```toml
[llm.ollama]
tool_use = true
```

When enabled, `OllamaProvider::supports_tool_use()` returns `true`. The agent switches to `chat_with_tools()`, which converts `ToolDefinition`s to `ollama_rs::ToolInfo`, sends them alongside the messages, and parses `tool_calls` blocks from the response. `ToolResult` message parts are sent back as `role: tool` messages.

When `tool_use = false` (the default), Ollama falls back to text-based extraction described below.

> [!NOTE]
> Requires a model that supports function calling (e.g. `qwen3:8b`, `llama3.1`, `mistral-nemo`). Check the Ollama model page to confirm tool support.

## Legacy Text Extraction

Providers without native tool support (Ollama with `tool_use = false`, Candle) use text-based tool invocation, distinguished by `InvocationHint` on each `ToolDef`:

1. **Fenced block** (`InvocationHint::FencedBlock("bash")` / `FencedBlock("scrape")`) — the LLM emits a fenced code block with the specified tag. `ShellExecutor` handles ` ```bash ` blocks, `WebScrapeExecutor` handles ` ```scrape ` blocks containing JSON with CSS selectors.
2. **Structured tool call** (`InvocationHint::ToolCall`) — the LLM emits a `ToolCall` with `tool_id` and typed `params`. `CompositeExecutor` routes the call to `FileExecutor` for file tools.

Both modes coexist in the same iteration. The system prompt includes invocation instructions per tool so the LLM knows exactly which format to use.

## ACP Tool Notifications

When Zeph runs inside an IDE via the [Agent Client Protocol](acp.md), tool execution emits structured session notifications that the IDE uses to display inline status.

### Lifecycle

Each tool invocation generates a UUID and sends two notifications:

| Notification | When | Content |
|-------------|------|---------|
| `SessionUpdate::ToolCall(InProgress)` | Before execution starts | Tool name, kind, UUID |
| `SessionUpdate::ToolCallUpdate(Completed\|Failed)` | After execution finishes | Full output text (`ContentBlock::Text`), file locations, UUID |

The UUID links both notifications so the IDE can update the same UI element — replacing a spinner with the result rather than creating two separate entries.

The output text in `ToolCallUpdate` is the `display` field from `LoopbackEvent::ToolOutput`, forwarded through `zeph-core`'s agent loop to the ACP channel. This is the same text that appears in the CLI output, after the output-filter pipeline and secret redaction have been applied.

### Tool kinds

The `kind` field on `ToolCall` tells the IDE what category of action to show:

| Tool | Kind |
|------|------|
| `bash`, `shell` | `Execute` |
| `read` | `Read` |
| `write`, `edit` | `Edit` |
| `search`, `grep`, `find` | `Search` |
| `web_scrape`, `fetch` | `Fetch` |
| everything else | `Other` |

### IDE terminal commands

Shell commands (`bash` tool) are routed through the IDE's native terminal via ACP `terminal/*` methods. This embeds the command output inside the IDE panel rather than running an invisible subprocess. See [terminal command timeout](acp.md#terminal-command-timeout) for timeout behaviour.

## DynExecutor

`DynExecutor` is a newtype wrapping `Arc<dyn ErasedToolExecutor>`. It implements `ToolExecutor` by delegating all methods through the erased trait, enabling a heap-allocated executor to be used wherever a concrete `ToolExecutor` is expected.

This is the mechanism that allows ACP sessions to supply IDE-proxied executors at runtime. The main binary wraps an ACP-aware composite in a `DynExecutor` and passes it to `AgentBuilder` — no changes to `Agent<C>` are needed for different tool backends.

```rust
let acp_composite = CompositeExecutor::new(acp_exec, local_exec);
let dyn_exec = DynExecutor(Arc::new(acp_composite));
agent_builder.with_tool_executor(dyn_exec);
```

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

When tool output exceeds a configurable character threshold, the full response is stored in the SQLite memory database (table `tool_overflow`) and the LLM receives a truncated version (head + tail split) with an opaque reference (`overflow:<uuid>`). This prevents large outputs from consuming the entire context window while preserving access to the complete data.

Overflow content is stored inside the main `zeph.db` database — no separate files are written to disk. Stale entries are cleaned up automatically on startup based on `retention_days`. Entries are also removed automatically via `ON DELETE CASCADE` when the parent conversation is deleted.

The `read_overflow` native tool allows the agent to retrieve a stored overflow entry by its UUID. The reference is intentionally opaque — no filesystem paths are exposed to the LLM. Retrieval is scoped to the current conversation: a query with a UUID that belongs to a different conversation returns `NotFound`, preventing cross-conversation data access.

### JIT retrieval

Large tool outputs are stored as references and injected into the context window on demand. When the agent sends a `read_overflow` call, the full content is loaded from SQLite at that point, rather than being kept resident in memory across turns. This keeps per-turn memory usage predictable regardless of how large previous tool outputs were.

### Configuration

```toml
[tools.overflow]
threshold = 50000       # Character count above which output is offloaded (default: 50000)
retention_days = 7      # Days to retain overflow entries before cleanup (default: 7)
max_overflow_bytes = 10485760  # Max bytes per entry (default: 10 MiB, 0 = unlimited)
```

### Security

- Overflow content is stored in the SQLite database, not on the filesystem — no path traversal risk.
- The reference returned to the LLM is a UUID (`overflow:<uuid>`), never a filesystem path.
- `read_overflow` validates the UUID format before querying the database.
- Overflow entries are scoped to the conversation they belong to and are deleted via CASCADE when the conversation is purged.
- Cross-conversation access is blocked at the query level: `load_overflow` requires both the UUID and the conversation ID to match.

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

## Anomaly detection

`AnomalyDetector` monitors tool failure rates in a sliding window. When the fraction of failed executions in the last `window_size` calls exceeds `failure_threshold`, a `Severity::Critical` alert is raised and the tool is automatically blocked via the trust system — no manual intervention required.

```toml
[tools.anomaly]
enabled = true
window_size = 20        # rolling window of last N executions
failure_threshold = 0.7 # 70% failures triggers Critical alert
auto_block = true       # block tool automatically on Critical
```

> [!NOTE]
> Auto-block via the trust system is reversible. A blocked tool can be unblocked by resetting its trust level. Anomaly events are logged via `tracing::warn!` with the tool name and failure rate.
