# MCP Security

## Overview

The Model Context Protocol (MCP) allows Zeph to connect to external tool servers via child processes or HTTP endpoints. Because MCP servers can execute arbitrary commands and access network resources, proper configuration is critical.

## SSRF Protection

Zeph blocks URL-based MCP connections (`url` transport) that resolve to private or reserved IP ranges:

| Range | Description |
|-------|-------------|
| `127.0.0.0/8` | Loopback |
| `10.0.0.0/8` | Private (Class A) |
| `172.16.0.0/12` | Private (Class B) |
| `192.168.0.0/16` | Private (Class C) |
| `169.254.0.0/16` | Link-local |
| `0.0.0.0` | Unspecified |
| `::1` | IPv6 loopback |

DNS resolution is performed before connecting, so hostnames pointing to private IPs (DNS rebinding) are also blocked.

## Safe Server Configuration

### Command-Based Servers

When configuring `command` transport servers, restrict the allowed executables:

```toml
[[mcp.servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/allowed/path"]
```

**Recommendations:**

- Only allow known, trusted executables
- Use absolute paths for commands when possible
- Restrict filesystem server paths to specific directories
- Avoid passing user-controlled input directly as command arguments
- Review server source code before adding to configuration

### URL-Based Servers

```toml
[[mcp.servers]]
id = "remote-tools"
url = "https://trusted-server.example.com/mcp"
```

**Recommendations:**

- Only connect to servers you control or explicitly trust
- Always use HTTPS — never plain HTTP in production
- Verify the server's TLS certificate chain
- Monitor server logs for unexpected tool invocations

## Per-Server Trust Model

Each `[[mcp.servers]]` entry has a `trust_level` field that controls tool exposure and SSRF enforcement:

| Trust Level | Tool Exposure | SSRF Checks |
|-------------|--------------|-------------|
| `trusted` | All tools | Skipped — operator asserts the server is safe |
| `untrusted` (default) | All tools | Applied |
| `sandboxed` | Only `tool_allowlist` entries | Applied — fail-closed |

**`trusted`** is intended for servers you fully control via static configuration (e.g., an internal tool server on `localhost`). SSRF validation is skipped for these servers.

**`untrusted`** (default) applies all SSRF validation rules and rate-limited tool list refreshes. A startup warning is emitted when `tool_allowlist` is empty, because the full tool set from an untrusted server is exposed without filtering.

**`sandboxed`** applies all SSRF rules and additionally filters tool discovery: only tools whose names appear in `tool_allowlist` are made available to the agent. An empty `tool_allowlist` with `trust_level = "sandboxed"` exposes zero tools (fail-closed). This is the safest configuration for external or third-party servers whose full tool catalog you do not trust.

```toml
# Minimal safe configuration for a third-party server
[[mcp.servers]]
id = "third-party"
url = "https://mcp.example.com/v1"
trust_level = "sandboxed"
tool_allowlist = ["search", "fetch_document"]
```

## Tool List Refresh Security

When an MCP server sends a `notifications/tools/list_changed` notification, Zeph fetches the updated tool list and passes it through `sanitize_tools()` before the tools are made available to the agent. This ensures that:

- Injection patterns introduced via a server-side tool list update are caught immediately.
- The sanitization invariant (sanitize before use) is maintained for both initial connection and all subsequent refreshes.

Refreshes are also rate-limited per server (minimum 5 seconds between refreshes) and capped at `MAX_TOOLS_PER_SERVER` (100) tools per server to limit the attack surface.

## Command Allowlist Validation

The `mcp.allowed_commands` setting restricts which binaries can be spawned as MCP stdio servers. Validation enforces:

- Only commands listed in `allowed_commands` are permitted (default: `["npx", "uvx", "node", "python", "python3"]`)
- **Path separator rejection**: commands containing `/` or `\` are rejected to prevent path traversal (e.g., `./malicious` or `/usr/bin/evil`)
- Commands must be bare names resolved via `$PATH`, not absolute or relative paths

## Environment Variable Blocklist

MCP server child processes inherit a sanitized environment. The following 21 environment variables (plus any matching `BASH_FUNC_*`) are stripped before spawning:

- Shell API keys: `ZEPH_CLAUDE_API_KEY`, `ZEPH_OPENAI_API_KEY`, `ZEPH_TELEGRAM_TOKEN`, `ZEPH_DISCORD_TOKEN`, `ZEPH_SLACK_BOT_TOKEN`, `ZEPH_SLACK_SIGNING_SECRET`, `ZEPH_A2A_AUTH_TOKEN`
- Cloud credentials: `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AZURE_CLIENT_SECRET`, `GCP_SERVICE_ACCOUNT_KEY`, `GOOGLE_APPLICATION_CREDENTIALS`
- Common secrets: `DATABASE_URL`, `REDIS_URL`, `GITHUB_TOKEN`, `GITLAB_TOKEN`, `NPM_TOKEN`, `CARGO_REGISTRY_TOKEN`, `DOCKER_PASSWORD`, `VAULT_TOKEN`, `SSH_AUTH_SOCK`
- Shell function exports: `BASH_FUNC_*` (glob match)

This prevents accidental secret leakage to untrusted MCP servers.

## Tool Collision Detection

When two connected MCP servers expose tools whose `sanitized_id` (server-prefix + normalized name) collide, Zeph logs a warning and the first-registered server's tool wins dispatch. This prevents a later server from silently shadowing an established tool.

Collision warnings appear at connection time and when a dynamic server is added via `/mcp add`. Check the log for `[WARN] mcp: tool id collision` lines if you suspect shadowing.

## Tool-List Snapshot Locking

By default, Zeph accepts `notifications/tools/list_changed` from connected servers and fetches an updated tool list. This creates a window for mid-session tool injection: a compromised or misbehaving server could swap in tools after the operator has reviewed the initial list.

Enable snapshot locking to prevent this:

```toml
[mcp]
lock_tool_list = true
```

When `lock_tool_list = true`, `tools/list_changed` notifications are rejected for all servers after the initial connection handshake. The tool set is frozen at connect time. The lock flag is applied atomically before the connection handshake to eliminate TOCTOU races.

## Per-Server Stdio Environment Isolation

By default, spawned MCP server processes inherit the full (already-sanitized) environment. For additional containment, enable per-server environment isolation:

```toml
# Apply to all stdio servers by default
[mcp]
default_env_isolation = true

# Override per server
[[mcp.servers]]
id = "sensitive-tools"
command = "npx"
args = ["-y", "@acme/sensitive"]
env_isolation = true
env = { TOOL_API_KEY = "vault:tool_key" }
```

With `env_isolation = true`, the child process receives only a minimal base environment (PATH, HOME, USER, TERM, TMPDIR, LANG, plus XDG dirs on Linux) plus the server-specific `env` map. All other inherited variables — including remaining secrets not caught by the blocklist — are stripped.

| Setting | Scope | Effect |
|---------|-------|--------|
| `default_env_isolation` | All stdio servers | Opt-in baseline for all servers |
| `env_isolation` per server | Single server | Override (can enable or disable the default) |

## Intent-Anchor Nonce Boundaries

Every MCP tool response is wrapped with a per-invocation nonce boundary:

```
[TOOL_OUTPUT::550e8400-e29b-41d4-a716-446655440000::BEGIN]
<tool output>
[TOOL_OUTPUT::550e8400-e29b-41d4-a716-446655440000::END]
```

The UUID is unique per call and generated inside Zeph, not from the server response. If tool output itself contains the string `[TOOL_OUTPUT::`, that prefix is escaped before wrapping, preventing injection attempts that mimic the boundary marker. This gives the injection-detection layer a reliable delimiter to trust.

## Elicitation Security

When a connected server uses the `elicitation/create` method to request user input, Zeph applies two safeguards:

1. **Phishing-prevention header** — the CLI always displays the requesting server's ID before showing any fields, so the user knows which server is asking.

2. **Sensitive field warning** — field names matching common secret patterns (password, token, secret, key, credential, auth, private, passphrase, pin) trigger an additional warning before the user is prompted. Configure with:

```toml
[mcp]
elicitation_warn_sensitive_fields = true   # default: true
```

`Sandboxed` trust-level servers are never allowed to elicit regardless of `elicitation_enabled`. This is enforced unconditionally.

## Environment Variables

MCP servers inherit environment variables from their configuration. Never store secrets directly in `config.toml` — use the [Vault](../security.md#age-vault) integration instead:

```toml
[[mcp.servers]]
id = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "vault:github_token" }
```
