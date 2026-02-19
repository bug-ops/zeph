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

## Command Allowlists

For production deployments, consider restricting which MCP tools can be invoked. While Zeph does not yet enforce tool-level allowlists, you can limit exposure by:

1. Running only the MCP servers you need
2. Configuring each server with minimal permissions
3. Using filesystem servers with read-only access where possible
4. Auditing tool calls via Zeph's tracing output (`RUST_LOG=zeph_mcp=debug`)

## Environment Variables

MCP servers inherit environment variables from their configuration. Never store secrets directly in `config.toml` — use the [Vault](../security.md#age-vault) integration instead:

```toml
[[mcp.servers]]
id = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "vault:github_token" }
```
