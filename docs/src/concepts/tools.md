# Tools

Tools give Zeph the ability to interact with the outside world. Three built-in tool types cover most use cases, with MCP providing extensibility.

## Shell

Execute any shell command via the `bash` tool. Commands are sandboxed:

- **Path restrictions**: configure allowed directories (default: current working directory only)
- **Network control**: block `curl`, `wget`, `nc` with `allow_network = false`
- **Confirmation**: destructive commands (`rm`, `git push -f`, `drop table`) require a y/N prompt
- **Output filtering**: test results, git diffs, and clippy output are automatically stripped of noise to reduce token usage

## File Operations

Five file tools (`read`, `write`, `edit`, `glob`, `grep`) provide structured access to the filesystem. All paths are validated against an allowlist. Directory traversal is prevented via canonical path resolution.

## Web Scraping

The `web_scrape` tool extracts data from web pages using CSS selectors. Configurable timeout (default: 15s) and body size limit (default: 1 MB).

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

## Deep Dives

- [Tool System](../advanced/tools.md) — full reference with filter pipeline, native tool use, iteration control
- [Security](../reference/security.md) — sandboxing and path validation details
