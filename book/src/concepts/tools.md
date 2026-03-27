# Tools

Tools give Zeph the ability to interact with the outside world. Three built-in tool types cover most use cases, with MCP providing extensibility.

## Shell

Execute any shell command via the `bash` tool. Commands are sandboxed:

- **Path restrictions**: configure allowed directories (default: current working directory only)
- **Network control**: block `curl`, `wget`, `nc` with `allow_network = false`
- **Confirmation**: destructive commands (`rm`, `git push -f`, `drop table`) require a y/N prompt
- **Output filtering**: test results, git diffs, and clippy output are automatically stripped of noise to reduce token usage
- **Detection limits**: indirect execution via process substitution, here-strings, `eval`, or variable expansion bypasses blocked-command detection; these patterns trigger a confirmation prompt instead

## File Operations

File tools provide structured access to the filesystem. All paths are validated against an allowlist. Directory traversal is prevented via canonical path resolution.

**Read/write:** `read`, `write`, `edit`, `grep`

**Navigation:** `find_path` (find files matching a glob pattern), `list_directory` (list entries with `[dir]`/`[file]`/`[symlink]` type labels)

**Mutation:** `create_directory`, `delete_path`, `move_path`, `copy_path` — all sandbox-validated, symlink-safe

## Web Scraping

Two tools fetch data from the web:

- **`web_scrape`** — extracts elements matching a CSS selector from an HTTPS page
- **`fetch`** — returns plain text from a URL without requiring a selector

Both tools share the same configurable timeout (default: 15s), body size limit (default: 1 MiB), and SSRF protection: private hostnames and IP ranges are blocked before any connection is made, DNS results are validated to prevent rebinding attacks, and HTTP redirects are followed manually (up to 3 hops) with each target re-validated. See [SSRF Protection for Web Scraping](../reference/security.md#ssrf-protection-for-web-scraping).

## Code Search

The `search_code` tool provides unified code intelligence: it combines semantic vector search (Qdrant), structural AST extraction (tree-sitter), and LSP symbol/reference resolution into a single agent-callable operation. Results are ranked and deduplicated across all three layers.

`search_code` is always available — `zeph-index` and tree-sitter are compiled into every build. Semantic vector search additionally requires Qdrant (`vector_backend = "qdrant"`) and an active code index (`[index] enabled = true`). Without Qdrant, the tool falls back to structural and LSP layers.

| Layer | Requires | Returns |
|-------|----------|---------|
| Structural (tree-sitter) | nothing | Symbol definitions with file/line |
| Semantic (Qdrant) | Qdrant + index | Ranked code chunks by meaning |
| LSP | mcpls MCP server | References, definitions, hover |

```
> find the authentication middleware
→ [structural] src/middleware/auth.rs:12 pub fn auth_layer
→ [semantic] src/middleware/auth.rs:45-87 (score: 0.91)
→ [lsp] 3 references found
```

See [Code Indexing](../advanced/code-indexing.md) for setup and configuration.

## Diagnostics

The `diagnostics` tool runs `cargo check` or `cargo clippy --message-format=json` and returns a structured list of compiler diagnostics (file, line, column, severity, message). Output is capped at a configurable limit (default: 50 entries) and degrades gracefully if `cargo` is absent.

## MCP Tools

Connect external tool servers via [Model Context Protocol](https://modelcontextprotocol.io/). MCP tools are embedded and matched alongside skills using the same cosine similarity pipeline — adding more servers does not inflate prompt size. See [Connect MCP Servers](../guides/mcp.md).

## Permissions

Three permission levels control tool access:

| Action | Behavior |
|--------|----------|
| `allow` | Execute without confirmation |
| `ask` | Prompt user before execution |
| `deny` | Block execution entirely |

Configure per-tool pattern rules in `[tools.permissions]`:

```toml
[[tools.permissions.bash]]
pattern = "cargo *"
action = "allow"

[[tools.permissions.bash]]
pattern = "*sudo*"
action = "deny"
```

First matching rule wins. Default: `ask`.

## Tool Error Taxonomy

When a tool call fails, Zeph classifies the error into one of 11 categories defined by `ToolErrorCategory`. The classification drives retry decisions, LLM parameter-reformat paths, and reputation scoring.

| Category | Retryable | Quality Failure | Description |
|----------|-----------|-----------------|-------------|
| `ToolNotFound` | no | yes | LLM requested a tool name not in the registry |
| `InvalidParameters` | no | yes | LLM provided invalid or missing parameters |
| `TypeMismatch` | no | yes | Parameter type mismatch (string vs integer, etc.) |
| `PolicyBlocked` | no | no | Blocked by security policy, sandbox, or trust gate |
| `ConfirmationRequired` | no | no | Operation requires user confirmation |
| `PermanentFailure` | no | no | HTTP 403/404 or equivalent permanent rejection |
| `Cancelled` | no | no | Cancelled by the user |
| `RateLimited` | yes | no | HTTP 429 or resource exhaustion |
| `ServerError` | yes | no | HTTP 5xx or equivalent server-side error |
| `NetworkError` | yes | no | DNS failure, connection refused, reset |
| `Timeout` | yes | no | Operation timed out |

**Quality failures** (`ToolNotFound`, `InvalidParameters`, `TypeMismatch`) trigger self-reflection — the LLM is shown a structured error and asked to correct its parameters. Infrastructure failures (`RateLimited`, `ServerError`, `NetworkError`, `Timeout`) are retried automatically and never trigger self-reflection.

When a tool call fails, the LLM receives a `ToolErrorFeedback` block instead of an opaque error string:

```
[tool_error]
category: invalid_parameters
error: missing required field: url
suggestion: Review the tool schema and provide correct parameters.
retryable: false
```

This structured format lets the LLM understand what went wrong and whether retrying with corrected parameters is appropriate. See [Tool System](../advanced/tools.md#tool-error-taxonomy) for the full reference.

## ErasedToolExecutor

The `ToolExecutor` trait is made object-safe via `ErasedToolExecutor`, enabling `Box<dyn ErasedToolExecutor>` for dynamic dispatch. This allows `Agent<C>` to hold any tool executor combination without a generic type parameter, simplifying the agent signature and making it easier to compose executors at runtime.

## Scheduler Tools

When the `scheduler` feature is enabled, three tools are injected into the LLM tool catalog:

| Tool | Description |
|------|-------------|
| `schedule_periodic` | Register a recurring task with a 5 or 6-field cron expression |
| `schedule_deferred` | Register a one-shot task to fire at a specific ISO 8601 UTC time |
| `cancel_task` | Cancel a scheduled task by name |

These tools are backed by `SchedulerExecutor`, which forwards requests over an mpsc channel to the background scheduler loop. See [Scheduler](scheduler.md) for the full reference.

## Think-Augmented Function Calling (TAFC)

TAFC enriches tool schemas for complex tools by injecting a `thinking` field that encourages the LLM to reason about parameter selection before committing to values. Tools with a complexity score above `complexity_threshold` (default: 0.6) are augmented automatically.

```toml
[tools.tafc]
enabled = true                # Enable TAFC schema augmentation (default: false)
complexity_threshold = 0.6    # Tools with complexity >= this are augmented (default: 0.6)
```

Complexity is computed from the number of required parameters, nesting depth, and enum cardinality. TAFC does not modify the tool's behavior — it only changes the JSON Schema presented to the LLM, adding a `thinking` string field where the model can reason step-by-step before selecting parameter values.

## Tool Schema Filtering

`ToolSchemaFilter` dynamically selects which tool definitions are included in the LLM context based on embedding similarity to the current query. Instead of sending all tool schemas on every turn (consuming tokens), only the most relevant tools are presented.

The filter integrates with the dependency graph: tools whose hard prerequisites have not yet been satisfied are excluded regardless of relevance score.

## Tool Result Cache

Idempotent tool calls within a session are cached to avoid redundant execution. The cache is keyed by tool name and a hash of the arguments. Non-cacheable tools (those with side effects like `bash`, `write`, `memory_save`, and all MCP tools) are excluded automatically.

```toml
[tools.result_cache]
enabled = true     # Enable tool result caching (default: true)
ttl_secs = 300     # Cache entry lifetime in seconds, 0 = no expiry (default: 300)
```

## Tool Dependency Graph

Configure sequential tool availability based on prerequisites. A tool with hard dependencies (`requires`) is hidden from the LLM until all prerequisites have completed successfully in the current session. Soft dependencies (`prefers`) add a similarity boost when satisfied.

```toml
[tools.dependencies]
enabled = true            # Enable dependency gating (default: false)
boost_per_dep = 0.15      # Similarity boost per satisfied soft dependency (default: 0.15)
max_total_boost = 0.2     # Maximum total boost from soft dependencies (default: 0.2)

[tools.dependencies.rules.deploy]
requires = ["build", "test"]   # Hard gate: deploy hidden until build and test complete
prefers = ["lint"]             # Soft boost: deploy scores higher if lint ran
```

This is useful for multi-step workflows where tool order matters (e.g., `read` before `edit`, `build` before `deploy`).

## Deep Dives

- [Tool System](../advanced/tools.md) — full reference with filter pipeline, native tool use, iteration control
- [Security](../reference/security.md) — sandboxing and path validation details
