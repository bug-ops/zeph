# Spec: MCP Client

## Sources

### External
- MCP specification (2025-11-25): https://modelcontextprotocol.io/specification/2025-11-25.md
- rmcp crate (Rust SDK): https://crates.io/crates/rmcp

### Internal
| File | Contents |
|---|---|
| `crates/zeph-mcp/src/` | `McpManager`, server lifecycle, tool registry |
| `crates/zeph-tools/src/composite.rs` | `McpExecutor` in composite chain |
| `crates/zeph-tui/src/channel.rs` | `suppress_stderr` integration |

---

`crates/zeph-mcp/` — Model Context Protocol client, multi-server lifecycle.

## Architecture

```
McpManager
├── servers: HashMap<String, McpServer>  — name → server instance
├── tool_registry: Qdrant-backed         — semantic tool search across all servers
└── suppress_stderr: bool                — must be true in TUI mode
```

## Server Lifecycle

1. **Startup** (`connect_all`) — two phases:
   - **Phase 1**: non-OAuth servers connect concurrently
   - **Phase 2**: OAuth servers connect sequentially (each may require browser interaction)
2. **Transport**: stdio child process (primary) or HTTP with optional auth (Mode A/B — see below)
3. **Capability negotiation**: `initialize` → `tools/list` → register in tool registry
4. **Health**: each server has a heartbeat; dead servers are restarted automatically
5. **Shutdown**: graceful SIGTERM → wait → SIGKILL fallback

## HTTP Authentication Modes

### Mode A — Static headers

Arbitrary request headers injected on every HTTP request. Header values support embedded vault
references (`${VAULT_KEY}`) resolved at startup.

```toml
[[mcp.servers]]
id = "todoist"
url = "https://api.todoist.com/mcp/v1"
[mcp.servers.headers]
Authorization = "Bearer ${TODOIST_API_TOKEN}"
```

- `headers` and `oauth.enabled` are mutually exclusive per server (validated at config load)
- Invalid header values (non-ASCII, control chars) are dropped with a `warn!` log

### Mode B — OAuth 2.1 with PKCE

Full authorization code flow with PKCE (S256). Implemented via rmcp `auth` feature.

```toml
[[mcp.servers]]
id = "todoist-oauth"
url = "https://ai.todoist.net/mcp"
[mcp.servers.oauth]
enabled = true
token_storage = "vault"   # "vault" | "memory"
callback_port = 18766     # localhost callback port
```

**Flow**:
1. Pre-bind `TcpListener` on `127.0.0.1:{callback_port}` before client registration (resolves redirect_uri port)
2. Discover OAuth metadata via RFC 8414 / RFC 9728 — all resolved endpoints validated via SSRF guard
3. Dynamic client registration (RFC 7591)
4. Display authorization URL: CLI via `eprintln!`, TUI via `status_tx` spinner "Waiting for OAuth...", Telegram via stderr
5. Callback server reads buffered TCP until `\r\n\r\n` (handles packet fragmentation)
6. Exchange code → tokens, persist via `VaultCredentialStore`
7. Subsequent runs: load tokens from vault, proactive TTL-based refresh

**Token vault keys**: `mcp.oauth.{server_id}.{access_token,refresh_token,expires_at}`

**Security**: PKCE S256 via rmcp, CSRF state validated and deleted after single use, callback binds `127.0.0.1` only, all OAuth metadata endpoints SSRF-checked

## Tool Discovery

- Tools from all MCP servers are merged into the main tool catalog each turn
- Qdrant-backed semantic registry: allows finding tools by description, not just exact name
- `McpExecutor` in `CompositeExecutor` handles routing of `ToolCall` to correct server
- Tool names are namespaced: `{server_name}__{tool_name}` to avoid collisions

## Protocol Invariants (MCP 2025-11-25)

- JSON-RPC 2.0 over stdio or HTTP+SSE
- `initialize` / `initialized` handshake required before any tool calls
- `tools/call` response must include `isError: bool` field
- Server stderr must be captured and suppressed in TUI mode — not propagated to user
- Tool input schema is JSON Schema — forwarded verbatim to LLM `chat_with_tools`

## Key Invariants

- `McpManager` is `Arc<>` — shared between agent loop and background restart tasks
- `suppress_stderr = true` is mandatory when TUI is active — failure to do so corrupts TUI output
- Tool names must be namespaced to prevent collisions across servers
- Server restart is automatic — agent must not error on transient server failures
- Qdrant tool registry is optional — falls back to exact-name lookup if Qdrant unavailable
- `headers` and `oauth.enabled` are mutually exclusive on the same server entry
- OAuth token storage uses `VaultCredentialStore` backed by age vault — tokens survive restarts
- Token refresh is proactive (TTL-based) only; no 401 retry (upstream rmcp limitation)
- `StoredCredentials` in rmcp does not implement `Zeroize` — accepted upstream limitation, vault at-rest encrypted
- All OAuth servers are skipped (with warning) on auth failure — non-fatal, matching existing server error behavior

## Sources Update

- [#1930](https://github.com/bug-ops/zeph/issues/1930) — OAuth 2.1 support for remote MCP servers (implemented in PR #1937)
- rmcp `auth` feature: `OAuthState`, `AuthorizationManager`, `VaultCredentialStore`, `InMemoryStateStore`
- MCP Authorization spec: https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization/
