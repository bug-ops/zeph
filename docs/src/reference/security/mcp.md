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

## Environment Variables

MCP servers inherit environment variables from their configuration. Never store secrets directly in `config.toml` — use the [Vault](../security.md#age-vault) integration instead:

```toml
[[mcp.servers]]
id = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "vault:github_token" }
```
