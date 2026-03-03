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

## ErasedToolExecutor

The `ToolExecutor` trait is made object-safe via `ErasedToolExecutor`, enabling `Box<dyn ErasedToolExecutor>` for dynamic dispatch. This allows `Agent<C>` to hold any tool executor combination without a generic type parameter, simplifying the agent signature and making it easier to compose executors at runtime.

## Scheduler Tools

When the `scheduler` feature is enabled, three tools are injected into the LLM tool catalog:

| Tool | Description |
|------|-------------|
| `schedule_periodic` | Register a recurring task with a 6-field cron expression |
| `schedule_deferred` | Register a one-shot task to fire at a specific ISO 8601 UTC time |
| `cancel_task` | Cancel a scheduled task by name |

These tools are backed by `SchedulerExecutor`, which forwards requests over an mpsc channel to the background scheduler loop. See [Scheduler](scheduler.md) for the full reference.

## Deep Dives

- [Tool System](../advanced/tools.md) — full reference with filter pipeline, native tool use, iteration control
- [Security](../reference/security.md) — sandboxing and path validation details
