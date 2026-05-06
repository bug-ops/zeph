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

## OS-Level Process Sandbox

In addition to file path allowlisting, shell commands executed by the agent run inside a platform-native subprocess isolation sandbox. This provides an additional defense layer against accidental or malicious file access and system calls.

### macOS: Seatbelt Profiles

On macOS, shell commands are wrapped with `sandbox-exec -f <profile>.sb -- <cmd>`. A Seatbelt profile is generated per-command (deny-default, explicit allow rules) from a `SandboxPolicy` configuration. The profile is written to a temporary file, passed to the kernel, and cleaned up after command completion.

**Default policy:**
- Deny all access
- Allow read/write only to explicitly configured paths
- Block `/private/tmp`, `/var/folders`, `/private/etc` (system directories)
- Optional network access control

**Configuration:**

```toml
[tools.sandbox]
allow_read = ["/home/user/projects", "/tmp"]
allow_write = ["/home/user/projects/build"]
allow_network = true
```

### Linux: Bubblewrap + Landlock + seccomp

On Linux (requires `sandbox` feature), commands are wrapped with `bwrap <ns-flags> <bind-mounts> --seccomp <fd> -- <cmd>`. Three isolation layers work together:

1. **Namespace isolation** — unshare UTS, IPC, PID (process tree), and optionally USER with UID/GID mapping
2. **Bind-mount filtering** — only paths listed in `allow_read`/`allow_write` are bind-mounted into the container; rest of filesystem is inaccessible
3. **seccomp BPF filter** — blocks 16 privilege-escalation syscalls (ptrace, execve-family variants, bpf, perf_event_open, etc.) via deny-list

Landlock filesystem rules (when available) provide an additional capability-based filter.

**Default policy:**
- Deny all access except read/write to configured paths
- Block network by default (enable with `allow_network = true`)
- Cannot escape via syscalls or ptrace

### Fallback: NoopSandbox

On platforms without support (Windows, or missing required tools), sandboxing is disabled with a warning. Commands run unsandboxed but file path allowlisting still applies via `FileExecutor`.

### Configuration

```toml
[tools.sandbox]
# disabled = false             # Set to true to disable sandboxing entirely (default: false)
# allow_read = []              # Paths/globs readable by commands (default: empty = cwd only)
# allow_write = []             # Paths/globs writable by commands (default: empty = cwd only)
# allow_network = true         # Allow outbound network (default: true)
```

### Best Practices

- **Minimize blast radius**: Configure `allow_read` and `allow_write` as tightly as possible. Empty lists restrict access to the current working directory only.
- **Project directories**: Allow read access to source trees and write access to build output directories.
- **Secrets**: Keep vault and config files outside the allowed paths; the sandbox cannot access them.
- **Debugging**: When sandbox violations occur, Zeph logs the denied syscall or path access. Check logs to refine the policy.

## WebScrapeExecutor — `fetch` tool

In addition to `web_scrape` (CSS-selector-based extraction), `WebScrapeExecutor` exposes a `fetch` tool that returns plain text from a URL without requiring a selector. SSRF validation (HTTPS-only, private IP block, redirect re-validation) is applied identically to both tools.

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | Yes | HTTPS URL to fetch |

## ShellExecutor — Background Shell Execution

The `bash` tool accepts an optional `background` parameter. When `true`, the command is spawned immediately and a stub message `[background] started run_id=<uuid>` is returned to close the LLM's `tool_use_id`. The actual completion arrives as a synthetic user message at the start of the next turn (drain-on-next-turn pattern).

```json
{
  "command": "cargo build --release",
  "background": true
}
```

Returns immediately:

```
[background] started run_id=abc-123
```

On the next turn, the completion is injected as a synthetic user-role message:

```
[background complete] run_id=abc-123 exit_code=0
<command output...>
```

This pattern decouples long-running operations from the prompt round-trip latency. The LLM can respond to the user or execute other tasks while the background process runs.

**Configuration:**

```toml
[tools.shell]
max_background_runs = 8           # maximum concurrent background tasks (default: 8)
background_timeout_secs = 1800    # timeout for background commands in seconds (default: 1800 = 30 minutes)
```

When a background task exceeds `background_timeout_secs`, it is killed and a completion stub with `exit_code=124` is sent on the next turn.

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

All providers use the native API-level tool mechanism for structured tool calling. `LlmProvider::supports_tool_use()` returns `true` by default. Tool definitions, execution, and result handling follow a single unified path.

In native mode:

- Tool definitions (name, description, JSON Schema parameters) are passed to the LLM API alongside the messages.
- The LLM returns structured `tool_use` content blocks with typed parameters.
- The agent executes each tool call and sends results back as `tool_result` messages.
- The system prompt instructs the LLM to use the structured mechanism, not fenced code blocks.

Types involved: `ToolDefinition` (name + description + JSON Schema), `ChatResponse` (Text or ToolUse), `ToolUseRequest` (id + name + input), and `ToolUse`/`ToolResult` variants in `MessagePart`.

Prompt caching is enabled automatically for Anthropic and OpenAI providers, reducing latency and cost when the system prompt and tool definitions remain stable across turns.

### Ollama

Ollama uses the same native tool calling path as Claude and OpenAI. `OllamaProvider` converts `ToolDefinition`s to `ollama_rs::ToolInfo`, sends them alongside the messages, and parses `tool_calls` blocks from the response. `ToolResult` message parts are sent back as `role: tool` messages.

> [!NOTE]
> Requires a model that supports function calling (e.g. `qwen3:8b`, `llama3.1`, `mistral-nemo`). Check the Ollama model page to confirm tool support.

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

## Per-Turn Execution Context

Each tool invocation receives a `ExecutionContext` that carries contextual information about the turn in which it is executing:

```rust
pub struct ExecutionContext {
    pub turn_id: String,           // UUID of the current agent turn
    pub goal_id: Option<String>,   // UUID of the active /plan goal (if any)
    pub skill_name: Option<String>,// Name of the active skill (if matched)
    pub timestamp_ms: u64,         // Unix timestamp of turn start
}
```

This context is available to tool executors via `ShellExecutor::context()` and can be used to:

- **Audit and tracing** — correlate tool invocations with the turn that triggered them
- **Goal-aware behavior** — adjust tool output based on the active goal or skill
- **Session reconstruction** — reconstruct the execution sequence from audit logs

Tool executors can opt-in to receiving the context:

```toml
[tools.shell]
enable_execution_context = true  # expose turn_id, goal_id, skill_name to hooks and auditing
```

When enabled, the context is propagated to shell command hooks (`hooks.file_changed`, `hooks.cwd_changed`) as environment variables:

| Variable | Source |
|----------|--------|
| `ZEPH_TURN_ID` | `ExecutionContext::turn_id` |
| `ZEPH_GOAL_ID` | `ExecutionContext::goal_id` (omitted if no active goal) |
| `ZEPH_SKILL_NAME` | `ExecutionContext::skill_name` (omitted if no active skill) |

## Goal Lifecycle and TACO Output Compression

When a `/plan` goal is active, tool outputs are subject to automatic compression via TACO (Tool-Aware Context Optimization). TACO uses a goal-aware compression strategy that:

1. **Preserves goal-relevant outputs** — tool results that directly address the active goal are never compressed
2. **Compresses tangential outputs** — results from exploratory or debugging tools outside the critical path are condensed into 2-3 line summaries
3. **Caches outputs** — compressed outputs are memoized so identical tool calls don't re-compress

**Goal lifecycle:**

When `/plan "Build a REST API"` is invoked:

1. A `TaskGraph` is created with UUID and stored in SQLite
2. Each tool invocation in the context of that plan gets `ExecutionContext::goal_id = <graph_id>`
3. At context assembly time, tool outputs are scored by relevance to the goal via:
   - Token count (smaller = more compressible)
   - Tool type (shell outputs compressed more aggressively than file reads)
   - Goal distance (proximity to the core task path)
4. When the goal completes, TACO stops applying compression and returns to normal tool output display

**Configuration:**

```toml
[tools.compression]
enabled = true
goal_aware = true              # Enable goal-aware compression (default: false)
compression_threshold_tokens = 300  # Compress outputs larger than this (default: 300)
preserve_shell_errors = true   # Never compress shell commands with exit_code != 0 (default: true)

# Compression strategies per tool type
[tools.compression.strategies]
bash = "aggressive"            # Compress shell output to 2-3 lines
read = "moderate"              # Keep file read outputs; only trim beyond 500 chars
web_scrape = "moderate"        # Keep scrape results; summarize only if > 1000 chars
find_path = "aggressive"       # Compress find results to "X files matching pattern"
```

When `goal_aware = true`, the compression strategy dynamically adjusts based on task relevance. A `grep` result that mentions the active goal's API function is preserved; one that mentions unrelated code is summarized.

**Example:**

```toml
# Without TACO
$ bash command: "cargo build --release"
[output: 50 lines of compiler messages]

$ read file: "src/lib.rs"
[output: 200 lines of source code]

# With TACO (goal_aware=true, active goal is "add error handling")
$ bash command: "cargo build --release"
[error handling additions: 3 relevant compiler messages; 47 others elided]

$ read file: "src/lib.rs"
[read src/lib.rs: 200 lines] (preserved because goal-adjacent; file reads not compressed)
```

## Capability Governance: TrajectorySentinel and ScopedToolExecutor

Tool execution can be gated by external security or governance policies. Two mechanisms work together:

### TrajectorySentinel

`TrajectorySentinel` observes the trajectory (sequence) of tool calls across a session and blocks calls that violate a learned policy. It learns patterns from:

- **Prior sessions** — tool sequences that caused errors, security violations, or policy breaches
- **User feedback** — when the user marks a tool result as "unacceptable" or "revoke", that sequence is marked as off-limits
- **Static allowlist** — tools listed in `[tools.governance]` are always available

Enable trajectory-based blocking:

```toml
[tools.governance]
trajectory_enabled = true
block_risky_patterns = true    # Default: false (off unless explicitly enabled)
blocked_sequences = [
  ["bash", "rm", "-rf", "/"],  # Never allow a full filesystem delete
  ["write", "config.toml", "password"],  # Never write credentials to config
]
```

The sentinel stores successful and failed sequences in SQLite and uses them to score subsequent invocations. A tool call can be blocked if:

- Its sequence matches a `blocked_sequences` entry
- Its sequence is semantically similar to a recent error sequence (via embedding similarity)

### ScopedToolExecutor

`ScopedToolExecutor` wraps an inner executor and applies permission checks before delegating. It enforces:

1. **Per-tool access control** — which tools can be invoked (allowlist or denylist)
2. **Per-parameter validation** — constraints on file paths, command content, URL domains
3. **Runtime permission escalation** — tools requiring higher trust level prompt the user before execution

```toml
[tools.scoped]
enabled = true

# Deny list: block specific tools
denied_tools = ["delete_path", "bash"]

# Allow list: only these tools are available (if set, denied_tools is ignored)
# allowed_tools = ["read", "write", "fetch"]

# Per-tool parameter constraints
[[tools.scoped.constraints]]
tool = "bash"
deny_patterns = ["rm -rf", "sudo", ":(){:|:|:|:}"]  # block dangerous commands

[[tools.scoped.constraints]]
tool = "write"
allowed_paths = ["/tmp", "/workspace"]  # only write to these directories
```

When a tool invocation violates a constraint, the agent receives an error message indicating which constraint was violated. The user can override with `/approve <tool_id>` if they trust the specific invocation.

Both mechanisms complement file path sandboxing and OS-level process sandboxing — they add policy enforcement at the Zeph orchestration layer.

## Per-Turn Execution Context

`ShellExecutor` maintains a per-turn `ExecutionContext` that persists across iterations within a single agent turn. This context includes:

- **Working directory** — set by the user or previous tool invocation; carries forward to subsequent commands
- **Environment variable overrides** — set via `export` or shell commands
- **Session history** — command history from previous iterations, available via shell history commands
- **Parsed state** — extracted values from previous tool outputs (e.g., URLs, file paths, parsed JSON)

The context is created at the start of each turn and discarded when the turn completes, ensuring tool outputs don't bleed into subsequent unrelated conversations.

```bash
> cd /path/to/project
[bash] cd /path/to/project

> cargo build
[bash] cargo build  # runs in /path/to/project (context persisted)

> find src -name "*.rs" | head
[bash] find src -name "*.rs" | head  # also runs in /path/to/project
```

## Goal Lifecycle and TACO Output Compression

When the agent is running toward an explicit goal (via `/plan` or `[agent] goal_text` config), tool outputs are evaluated for relevance to that goal. TACO (Token-Aware Compression Orchestration) applies goal-aware output filtering that removes off-topic information.

During each tool invocation:

1. **Goal relevance scoring** — TACO scores the tool output for relevance to the current goal using embedding similarity
2. **Compression** — Off-topic sections are replaced with `[output filtered: <reason>]` placeholders
3. **Preservation** — Output directly matching the goal or containing errors is always preserved

Enable TACO by setting a goal:

```bash
> /plan Implement authentication middleware for the REST API
```

Configuration for compression thresholds:

```toml
[tools.compression]
goal_relevance_threshold = 0.5    # Skip sections with relevance < 0.5
preserve_errors = true            # Always keep error messages
max_preserved_chars = 4096        # Hard limit on preserved output size
```

When no goal is active, TACO is disabled and all tool output is preserved.

## TrajectorySentinel and ScopedToolExecutor

To prevent tool misuse and enforce capability governance, Zeph optionally wraps executors with `TrajectorySentinel` (tracks execution patterns) and `ScopedToolExecutor` (enforces per-user scope and trust levels).

`ScopedToolExecutor` ensures that:

- **Per-user scope** — tools run as the configured user (e.g., `www-data` for web services), not the agent process owner
- **Trust delegation** — sensitive tools (e.g., `rm`, `sudo`) require an elevated trust level
- **Capability auditing** — all tool invocations are logged with user, timestamp, and scope context

Enable scoped execution via `[tools.scope]`:

```toml
[tools.scope]
enabled = true
run_as_user = "zeph"          # Execute tools as this user (via sudo if needed)
require_capability = false    # Require elevated permissions
audit_all_invocations = true  # Log every tool call
```

When enabled, the executor constructs a `ToolScope` binding the user identity, permission level, and audit context. The scope is passed through all tool execution layers — file access, shell commands, and MCP tools are all aware of and respect the scope.

> [!WARNING]
> Scope enforcement requires the agent to run with sufficient privileges (typically `root` or via `sudo`) to switch user contexts. Running as an unprivileged user with `run_as_user = "other-user"` will fail with a permission error.

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

## Structured Shell Output Envelope

When `execute_bash` completes, stdout and stderr are captured as separate streams using a tagged channel. The result is stored as a `ShellOutputEnvelope` in `ToolOutput.raw_response`:

```json
{
  "stdout": "...",
  "stderr": "...",
  "exit_code": 0,
  "truncated": false
}
```

The LLM context continues to receive the interleaved combined output (in `summary`) — behavior for the agent is unchanged. ACP and audit consumers, however, can access the envelope directly via `raw_response` to distinguish stdout from stderr and inspect the exact exit code.

`AuditEntry` gains two optional fields populated from the envelope:

| Field | Description |
|-------|-------------|
| `exit_code` | Process exit code (`null` when the process was killed by a signal) |
| `truncated` | `true` when output was cut to the overflow threshold |

## File Read Sandbox

`FileExecutor` supports a per-path read sandbox via `[tools.file]`:

```toml
[tools.file]
deny_read  = ["/etc/shadow", "/root/*", "/home/*/.ssh/*"]
allow_read = ["/etc/hostname"]
```

Evaluation order: deny-then-allow. Patterns are matched against canonicalized absolute paths, so symlinks pointing into a denied directory are still blocked after resolution.

See the [File Read Sandbox](../reference/security/file-sandbox.md) reference for the full configuration and glob syntax.

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

## Think-Augmented Function Calling (TAFC)

TAFC augments the JSON Schema of complex tools with a `thinking` field that encourages step-by-step reasoning before the LLM selects parameter values. This reduces parameter selection errors for tools with many required parameters, deeply nested schemas, or large enum cardinalities.

### How It Works

1. Each tool definition is scored for complexity based on: number of required parameters, nesting depth, and enum cardinality.
2. Tools with complexity >= `complexity_threshold` (default: 0.6) have their JSON Schema augmented with a `thinking` string property.
3. The LLM fills the `thinking` field first (reasoning about the task), then fills the actual parameters. The `thinking` value is discarded before execution.

### Configuration

```toml
[tools.tafc]
enabled = true                # Enable TAFC augmentation (default: false)
complexity_threshold = 0.6    # Complexity score threshold (default: 0.6)
```

The threshold is validated and clamped to [0.0, 1.0]; NaN and Infinity are reset to 0.6.

## Tool Schema Filtering

`ToolSchemaFilter` dynamically selects which tool definitions are included in the LLM context on each turn. Instead of sending all tool schemas every time, only tools with embedding similarity above a threshold to the current query are included. This significantly reduces token usage when many tools are registered.

The filter integrates with the tool dependency graph: tools whose hard prerequisites (`requires`) have not been satisfied are excluded from the filtered set regardless of relevance score. The `DependencyExclusion` metadata is attached to each filtered-out tool for observability.

## Tool Result Cache

The tool result cache stores outputs of idempotent tool calls within a session. When the same tool is called with identical arguments, the cached result is returned immediately without re-execution.

### Cacheability Rules

- **Always non-cacheable:** `bash` (side effects), `write` (file mutation), `memory_save` (state mutation), `scheduler` (task creation), and all MCP tools (`mcp_` prefix, opaque third-party)
- **Non-cacheable by exclusion:** `memory_search` (results may change after `memory_save`)
- **Cacheable:** `read`, `edit`, `grep`, `find_path`, `list_directory`, `web_scrape`, `fetch`, `diagnostics`, `search_code`

### Configuration

```toml
[tools.result_cache]
enabled = true     # Enable result caching (default: true)
ttl_secs = 300     # Cache entry lifetime in seconds, 0 = no expiry (default: 300)
```

Cache entries are keyed by `(tool_name, hash(args))` and expire after `ttl_secs`. The cache is in-memory only — it does not persist across session restarts.

## Tool Dependency Graph

The tool dependency graph controls tool availability based on prerequisites. Two dependency types are supported:

| Type | Behavior |
|------|----------|
| `requires` (hard) | Tool is **hidden** from the LLM until all listed tools have completed successfully |
| `prefers` (soft) | Tool receives a **similarity boost** when listed tools have completed |

### Configuration

```toml
[tools.dependencies]
enabled = true            # Enable dependency gating (default: false)
boost_per_dep = 0.15      # Boost per satisfied soft dependency (default: 0.15)
max_total_boost = 0.2     # Maximum total soft boost (default: 0.2)

[tools.dependencies.rules.deploy]
requires = ["build", "test"]
prefers = ["lint"]

[tools.dependencies.rules.edit]
requires = ["read"]
```

When a hard dependency is not yet satisfied, the tool is excluded from the `ToolSchemaFilter` output and does not appear in the LLM's tool catalog. The `DependencyExclusion` metadata records which dependency was unsatisfied, visible in debug logs.

## Tool Error Taxonomy

Every tool failure is classified into one of 11 `ToolErrorCategory` values. Classification drives three independent recovery mechanisms:

| Mechanism | Triggered by |
|-----------|-------------|
| Automatic retry with backoff | `RateLimited`, `ServerError`, `NetworkError`, `Timeout` |
| LLM parameter-reformat path | `InvalidParameters`, `TypeMismatch` |
| Reputation scoring / self-reflection | `InvalidParameters`, `TypeMismatch`, `ToolNotFound` |

### ToolError::Shell

Shell tool failures carry an explicit `category` field and exit code:

```rust
ToolError::Shell {
    exit_code: Option<i32>,
    category: ToolErrorCategory,
}
```

The category is derived from the exit code and OS error kind via `classify_io_error`. An OS-level `NotFound` (command not found) maps to `PermanentFailure`, not `ToolNotFound` — `ToolNotFound` is reserved for registry misses where the LLM requested a tool name that does not exist.

### ToolErrorFeedback

On any classified failure, the executor injects a `ToolErrorFeedback` block as the `tool_result` content instead of an opaque error string:

```
[tool_error]
category: rate_limited
error: too many requests
suggestion: Rate limit exceeded. The system will retry if possible.
retryable: true
```

`format_for_llm()` produces this four-line block. The `retryable` flag tells the LLM whether the system will retry automatically so it does not need to ask for the operation to be repeated.

### HTTP Status Classification

`classify_http_status(status)` maps HTTP codes to categories:

| HTTP Status | Category |
|-------------|----------|
| 400, 422 | `InvalidParameters` |
| 401, 403 | `PolicyBlocked` |
| 429 | `RateLimited` |
| 500–599 | `ServerError` |
| 404, 410, others | `PermanentFailure` |

### Infrastructure vs Quality Failures

The taxonomy enforces a hard split:

- **Infrastructure failures** (`RateLimited`, `ServerError`, `NetworkError`, `Timeout`) are never quality failures. They must not trigger self-reflection — the failure is not attributable to LLM output.
- **Quality failures** (`InvalidParameters`, `TypeMismatch`, `ToolNotFound`) indicate the LLM produced incorrect tool invocations. A single parameter-reformat attempt is made before the failure is final.

## MCP Error Codes

`McpErrorCode` classifies MCP tool call failures for caller-side retry decisions without requiring string parsing:

| Code | `is_retryable()` | Description |
|------|-----------------|-------------|
| `Transient` | `true` | Temporary failure; retry is likely to succeed |
| `RateLimited` | `true` | Server-side rate limit; back off before retrying |
| `InvalidInput` | `false` | Bad parameters; retry without input change would fail |
| `AuthFailure` | `false` | Authentication or authorization failure |
| `ServerError` | `true` | Internal server error; may succeed on retry |
| `NotFound` | `false` | Tool or resource does not exist |
| `PolicyBlocked` | `false` | Blocked by local policy enforcer |

`McpError::ToolCall` carries a `code: McpErrorCode` field. `McpError::code()` maps all error variants to typed codes.

## Caller Identity Propagation

Every tool call carries an optional `caller_id: Option<String>` field that is populated from the channel layer (e.g. Telegram user ID, ACP session ID) and propagated to the audit log. `AuditEntry` gains two additional fields:

| Field | Description |
|-------|-------------|
| `caller_id` | Opaque identifier of the invoking principal; `null` for CLI sessions |
| `policy_match` | The `PolicyDecision::trace` from the allow/deny decision; `null` when no policy matched |

Both fields are omitted from the JSON audit log when `null`.

## Per-Session Tool Call Quota

Limit the total number of tool executions per session to prevent runaway agent loops or cost overruns.

```toml
[tools]
max_tool_calls_per_session = 50   # Maximum tool calls allowed per session (default: unset = unlimited)
```

The counter increments once per logical batch (not per retry). When the quota is exhausted, all calls in the batch return a synthetic `quota_blocked` error without executing. The counter resets when the user runs `/clear`.

## OAP Authorization Config

In addition to the declarative `[tools.policy]` rules, a supplementary authorization layer can be configured via `[tools.authorization]`. Rules from this section are merged into `PolicyEnforcer` after the `policy.rules` entries (policy takes precedence — first-match-wins).

```toml
[tools.authorization]
enabled = true

[[tools.authorization.rules]]
effect = "deny"
tool   = "bash"
args_match = ".*sudo.*"

[[tools.authorization.rules]]
effect = "allow"
tool   = "read"
paths  = ["/home/*"]
```

`PolicyRuleConfig` accepts the same fields as `[[tools.policy.rules]]` (see [Policy Enforcer](policy-enforcer.md)). A `capabilities` field is reserved for future use when tools expose capability metadata.

> [!NOTE]
> `[tools.authorization]` requires the `policy-enforcer` feature. It is disabled by default even when the feature is compiled in.

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
