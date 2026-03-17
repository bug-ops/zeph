# Spec: ACP (Agent Client Protocol)

## Sources

### External
- ACP specification: https://agentclientprotocol.com/get-started/introduction
- ACP Rust SDK: https://github.com/agentclientprotocol/rust-sdk
- `agent-client-protocol` crate: https://crates.io/crates/agent-client-protocol

### Internal
| File | Contents |
|---|---|
| `crates/zeph-acp/src/lib.rs` | Public API, `AgentSpawner`, `AcpContext` |
| `crates/zeph-acp/src/transport/stdio.rs` | stdio transport |
| `crates/zeph-acp/src/transport/http.rs` | HTTP+SSE transport |
| `crates/zeph-acp/src/transport/ws.rs` | WebSocket transport |
| `crates/zeph-acp/src/transport/auth.rs` | Bearer token auth |
| `crates/zeph-acp/src/transport/router.rs` | axum router |
| `crates/zeph-acp/src/permission.rs` | `AcpPermissionGate`, TOML persistence |
| `crates/zeph-acp/src/agent/mod.rs` | Session lifecycle, `AgentSpawner` |
| `crates/zeph-acp/src/fs.rs` | `resolve_resource_link`, SSRF/path checks |
| `crates/zeph-acp/src/mcp_bridge.rs` | MCP passthrough |

---

`crates/zeph-acp/` (feature: `acp`) — ACP v0.12.0+, enables IDE integration.

## Transports

| Transport | Feature | Notes |
|---|---|---|
| stdio | `acp` (base) | Primary; mutually exclusive with TUI |
| HTTP + SSE | `acp-http` | axum server, SSE for streaming |
| WebSocket | `acp` | tokio-tungstenite |

- ACP stdio and TUI are **mutually exclusive** — both own stdin/stdout
- Enforced at startup: attempting both → hard error with clear message

## Session Model

```
AcpSessionManager
├── sessions: LruCache<SessionId, AcpSession>  — bounded by max_sessions
├── max_sessions: usize                         — default 10
└── eviction: LRU policy
```

- Sessions are stateful: each has its own conversation history + tool context
- **LRU eviction**: oldest unused session is dropped when capacity is reached
- Session fork: create a new session branching from an existing session at a given turn
- Session resume: reconnect to an existing session by ID

## Permission Model

```
AcpPermissionGate (TOML-backed, SQLite-persisted)
├── per-tool rules: Simple("allow"|"deny") | Patterned { default, patterns }
└── persistence: survives process restart
```

- Permissions stored in TOML config dir, loaded at startup
- For shell tools: extracts binary name (skips transparent prefixes: `env`, `exec`, `nice`, `nohup`, `time`)
- Patterns: `git = "allow"`, `rm = "deny"` — applied to binary names
- Async request queue: async lookup with oneshot reply channels — agent blocked until user answers
- Tool call lifecycle: `proposed → approved/denied → persisted → executed → result`

## Protocol Messages (ACP v0.12.0+)

- Rich content: images, file resources, binary data
- Model switching: client can request a specific model per session
- Terminal forwarding: tool output streams back to IDE terminal
- File tools: read/write/list within session working directory
- MCP passthrough: MCP tools are forwarded to ACP client via `mcp_passthrough` capability

## Unstable Features (feature: `acp-unstable`)

- `unstable-session-list`: enumerate active sessions
- `unstable-session-fork`: fork session at a point
- `unstable-session-resume`: resume by session ID

## Resource Link Rules (`resolve_resource_link`)

- `file://` URIs: canonicalize (resolve symlinks), must be under `session_cwd`
  - Reject: `/proc`, `/sys`, `/dev`, `/.ssh`, `/.gnupg`, `/.aws`
  - Null byte in content → treat as binary → reject
- `http(s)://` URIs: no redirects; post-fetch IP check (fail-closed on missing remote_addr)
  - Reject private IPs (SSRF protection)
  - Text-only MIME, 1 MiB limit, 10s timeout
  - Validate UTF-8 before returning

## Config Coverage

ACP mode uses the same `config/default.toml` and the same resolution order as CLI/TUI
(see `020-config-loading/spec.md`). However, not all config sections affect ACP agent
behavior. The table below is the authoritative source of truth.

| Config section | ACP status | Reason |
|---|---|---|
| `[agent]` | **Active** | Core agent identity, model, system prompt |
| `[llm]` | **Active** | Provider selection, model, token limits |
| `[skills]` | **Active** | Skill registry, matching thresholds |
| `[memory]` | **Active** | SQLite + Qdrant, recall, summarization |
| `[tools]` | **Active** | Shell executor, web scrape, audit |
| `[vault]` | **Active** | Secret resolution (same as all modes) |
| `[mcp]` | **Active** | MCP servers are wired in ACP sessions |
| `[acp]` | **Active** | ACP-specific: bind, auth, sessions, permissions |
| `[logging]` | **Active** | Logging config applied at early bootstrap |
| `[scheduler]` | **Active (config only)** | Executor wired; `--scheduler-disable` / `--scheduler-tick` CLI flags are **not available** in ACP — use config fields only |
| `[skills.learning]` | **Ignored** | Self-learning requires a session feedback loop not present over ACP; `judge_provider` is built but `.with_learning()` is not called |
| `[index]` | **Ignored** | Code indexing is an interactive CLI/TUI feature; not applicable per-session over ACP |
| `[lsp]` | **Ignored** | LSP hook injection is not wired in ACP agent initialization |
| `[agents]` | **Ignored** | Subagent delegation is not supported in ACP sessions |
| `[orchestration]` | **Ignored** | DAG planner and AgentRouter are not wired for ACP |
| `[cost]` | **Ignored** | Cost tracking not applied; ACP clients are expected to manage their own token budgets |
| `[experiments]` | **Ignored** | Benchmarking and eval sessions are not applicable in ACP mode |
| `[gateway]` | **Ignored** | HTTP webhook ingestion is spawned by `runner.rs` independently of ACP sessions |
| `[telegram]` / `[discord]` / `[slack]` | **Ignored** | ACP uses `LoopbackChannel` — external chat channels do not apply |

### Code annotation requirement

`build_acp_deps()` and `spawn_acp_agent()` in `src/acp.rs` **must** contain an explicit
comment block that mirrors the "Ignored" rows above, with a one-line reason per section.
This ensures the divergence is visible to any developer editing the initialization path.

**NEVER** silently drop a config section in ACP without updating this table first.

## Key Invariants

- ACP stdio transport is always mutually exclusive with TUI — enforced at startup
- Session IDs are stable UUIDs — never reassigned or reused after expiry
- LRU eviction is by last-access time, not creation time
- `file://` resource paths must stay under `session_cwd` — no `..` escape
- Null byte in file content = binary → reject unconditionally
- Bearer token comparison is constant-time (BLAKE3 + `ct_eq`) — never `==`
- MCP passthrough requires `mcp` crate active — verify capability at negotiation time
