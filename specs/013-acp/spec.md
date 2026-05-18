---
aliases:
  - ACP
  - Agent Client Protocol
tags:
  - sdd
  - spec
  - protocol
  - acp
created: 2026-04-08
updated: 2026-05-19
status: approved
related:
  - "[[MOC-specs]]"
  - "[[014-a2a/spec]]"
---

# Spec: ACP (Agent Client Protocol)

> [!info]
> ACP transports, session management, permissions, fork/resume,
> capability advertisement, agent-client-protocol 0.12.1 / schema 0.13.2 compatibility.

## Spec Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-04-08 | sdd | Initial spec (SDK 0.11.1 / schema 0.12.0) |
| 1.1 | 2026-05-19 | sdd | Updated to SDK 0.12.1 / schema 0.13.2; added Providers API, Elicitation, MCP-over-ACP, Session Usage, Session Delete migration, v2 tracking, breaking changes resolution |

---

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

`crates/zeph-acp/` (feature: `acp`) — enables IDE integration via Agent Client Protocol.

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

### Agent Spawner Contract (0.12.1)

Agent sessions use the `Agent.builder()` / `run_agent()` pattern from
`agent-client-protocol 0.11.1`, preserved in 0.12.1. Session state is `Arc`-wrapped.
Session tasks are launched via `tokio::task::spawn_local` inside a `LocalSet` — the
`AgentSpawner` closure returns `Pin<Box<dyn Future<Output = ()> + 'static>>` (`!Send`).

SDK 0.12.0 removed `McpAcpTransport` and the direct `tokio` re-export. Zeph is unaffected:
`McpAcpTransport` was never used, and Zeph has its own `tokio` dependency.

`session/close` and `session/resume` were stabilized in SDK 0.12.0 (schema 0.12.2).
The `unstable-session-resume` and `unstable-session-close` feature flags in Zeph should be
removed after SDK upgrade to 0.12.1.

**Status: not-implemented** (SDK still pinned to 0.11.1; upgrade tracked as implementation gap I1)

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

## Protocol Messages

- Rich content: images, file resources, binary data
- Model switching: client can request a specific model per session
- Terminal forwarding: tool output streams back to IDE terminal
- File tools: read/write/list within session working directory
- MCP passthrough: MCP tools are forwarded to ACP client via `mcp_passthrough` capability

## Configuration

ACP behavior is configured via the `[acp]` section in `config.toml`. The following fields
are available in PR4+:

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable ACP server |
| `agent_name` | String | `"zeph"` | Agent name advertised to clients |
| `transport` | String | `"stdio"` | Transport: `stdio`, `http`, `ws`, `both` |
| `additional_directories` | `Vec<String>` | `[]` | **Request-side allowlist.** Paths a client may pass in `sessionInit.additionalDirectories`. Paths not in this list are rejected at session start. This is NOT a protocol advertisement — it is a server-side gate. |
| `auth_methods` | `Vec<String>` | `["agent"]` | Accepted authentication methods. MVP: only `"agent"` is valid. Unknown values are rejected at deserialization. |
| `message_ids_enabled` | bool | `true` | Echo client-supplied `message_id` in `PromptResponse.user_message_id` and all streamed chunks. |

### Key Invariants

- `additional_directories` is a **request-side allowlist**: paths requested by the client must be
  a prefix of a configured allowed path; requests with non-allowed paths are rejected with
  `AcpError::PermissionDenied` at session start — never silently ignored
- `auth_methods` must only contain `"agent"` for MVP; unknown variants cause a hard deserialization
  error at startup to prevent misconfigured deployments from silently accepting unexpected auth
- When `message_ids_enabled = true`, every `PromptResponse` and every streamed chunk must carry the
  originating `message_id` — partial echo (response but not chunks, or vice versa) is a bug

## Session CRUD Endpoints (#3902, #4252)

ACP exposes REST-style endpoints for session lifecycle management alongside the existing WebSocket/SSE protocol paths.

| Method | Path | Description |
|--------|------|-------------|
| `POST /sessions` | Create new session | Returns `{ session_id, status }` |
| `GET /sessions/{id}` | Fetch session metadata | Returns current status, `working_dir`, created_at |
| `PATCH /sessions/{id}` | Update session (partial update) | Supports `working_dir` update |
| `DELETE /sessions/{id}` | Terminate session | Graceful teardown (same as `session/close`) |

### SessionStatus Enum

```
running  — session is active and processing messages
idle     — session is open but waiting for input
stopped  — session has been gracefully terminated
error    — session terminated due to an unhandled error
```

`SessionStatus` is `#[non_exhaustive]` — callers must handle unknown variants gracefully.

### PATCH working_dir rules

- The new `working_dir` must be within the `additional_directories` allowlist (same gate as session init)
- Path is canonicalized via `tokio::fs::canonicalize` — no blocking worker threads
- Paths outside the allowlist return `403 Forbidden` — never silently accepted

### Key Invariants

- `POST /sessions` response is synchronous — the session is ready to accept messages before the response returns
- `DELETE /sessions/{id}` follows the same flush-then-remove contract as `session/close`
- `GET /sessions/{id}` returns 404 after `DELETE` — session IDs are not reused

---

## Stable Features

### session/close

**Status: stable** (stabilized in schema 0.12.2, SDK 0.12.0)

`session/close` handler gracefully terminates an ACP session: flushes pending memory writes,
cancels in-flight tool calls, persists session state to SQLite, and removes the session from
the LRU cache. Previously named `session/stop` (renamed in schema 0.11.2).

The `reason` field on `session/close` is now part of the stable API; it carries a human-readable
string for diagnostics (e.g., `"user_initiated"`, `"timeout"`, `"error"`).

#### Key Invariants

- `session/close` must flush all pending writes before removing the session — no data loss on close
- In-flight tool calls receive a cancellation signal; callers must handle `ToolError::Cancelled`
- Session ID is invalidated after close — subsequent requests with the same session ID return 404

### session/resume

**Status: stable** (stabilized in schema 0.12.2, SDK 0.12.0)

Reconnect to an existing session by ID, restoring conversation history and tool context.
Previously gated behind `unstable-session-resume` feature flag in Zeph.

**Action required on SDK upgrade**: remove `unstable-session-resume` feature flag from
`crates/zeph-acp/Cargo.toml` and root `Cargo.toml`. Use the stable API directly.

### Capability Negotiation

**Status: stable**

ACP server advertises its capabilities in the `initialize` response and via the `/agent.json` endpoint.

#### /agent.json Endpoint

`GET /agent.json` returns a JSON document describing the agent's identity, declared capabilities, supported protocol version, and authentication methods. This endpoint is unauthenticated and used by IDE clients for discovery.

```json
// after SDK upgrade to 0.12.1 / schema 0.13.2 (see I1)
{
  "name": "...",
  "version": "...",
  "protocol": "acp/0.13.2",
  "capabilities": ["tools", "memory", "streaming"],
  "authMethods": ["bearer"]
}
```

#### Protocol Version

Zeph currently uses `agent-client-protocol 0.11.1` / `schema 0.12.0`. Target version is
SDK 0.12.1 / schema 0.13.2. The `/agent.json` `protocol` field must be updated to match
the compiled crate version after the SDK upgrade.

#### Current Model in SessionInfoUpdate

`SessionInfoUpdate` messages include the `current_model` field so clients can display which
LLM model is active for the session. Also exposed in `session/list` response. The provider
field in relevant messages is now optional (stabilized in SDK 0.12.1 schema 0.12.1).

#### Key Invariants

- `/agent.json` is always unauthenticated — bearer token must NOT be required for this endpoint
- `authMethods` in `/agent.json` must reflect the actual authentication configuration — never hardcoded
- IPI duplication between ACP session init and MCP passthrough is eliminated — validate once, not twice
- Protocol version in `/agent.json` must match the compiled `agent-client-protocol` crate version

### Input Schemas for Tools

**Status: stable**

Tool definitions include `inputSchema` (JSON Schema) describing accepted parameters. ACP clients
use this for type-safe invocation. Zeph's tool definitions must populate `inputSchema` when
exposing tools over ACP.

---

## Unstable Features (feature: `acp-unstable`)

- `unstable-session-list`: enumerate active sessions *(was already stable at 0.11.1)*
- `unstable-session-fork`: fork session at a point

> **Note**: `unstable-session-resume` and `unstable-session-close` are no longer unstable
> upstream (stabilized in schema 0.12.2). These flags should be removed after SDK upgrade to 0.12.1.

---

## New Protocol Features

### Providers API

**Status: design-needed**

Schema 0.11.7 introduced a providers management API (`unstable` in SDK):

| Method | Description |
|--------|-------------|
| `providers/list` | Returns available LLM providers for the session |
| `providers/set` | Sets the active provider for the session |
| `providers/disable` | Disables a provider for the session |

**Design note — impedance mismatch**: The Providers API is NOT a direct mapping to Zeph's
`[[llm.providers]]` TOML config. Key tensions:

1. **Startup resolution**: Zeph resolves providers at startup from the age vault. ACP providers
   are runtime-dynamic (client can set/disable per session). These are different lifecycles.
2. **Identity scheme**: ACP providers are identified by a provider ID string. Zeph's
   `[[llm.providers]]` uses a `name` field that is an internal reference, not an ACP-visible identity.
3. **Per-session override**: It is unclear whether `providers/set` should override the global
   provider for the session only, or affect the global registry. This requires an explicit
   architectural decision.
4. **`providers/disable` scope**: Does disabling a provider affect only the ACP session, the
   global registry, or the vault-resolved config?

**Open questions**:
- What is the ACP provider identity scheme? Is it the Zeph `name` field or something else?
- Should `providers/list` enumerate only providers active for the current session, or all
  configured providers?
- Should the client be able to add new providers dynamically (not in the TOML config)?

**Acceptance criteria** (for implementation):
- `providers/list` returns providers visible to the current ACP session, with their current status
- `providers/set` overrides the provider for the current session only — does not affect global config
- `providers/disable` disables a provider for the current session only
- Provider changes survive within the session but are not persisted after `session/close`
- Vault-resolved keys are never exposed in `providers/list` response

---

### Elicitation Protocol

**Status: design-needed**

Schema 0.11.5 introduced structured user input (elicitation) across three scopes:
- **Session scope** (0.11.5, PR #792): agent requests structured input during session initialization
- **Tool call scope** (0.11.5, PR #769): agent requests structured input before executing a tool
- **Request scope** (0.11.5, PR #771): agent requests structured input during prompt processing
- **Scoped by mode** (0.11.6, PR #966): elicitation behavior varies by mode

**Current Zeph state**: `unstable-elicitation = []` in `crates/zeph-acp/Cargo.toml` is a
**local empty feature flag** — it does NOT pass through to `agent-client-protocol/unstable_elicitation`.
The SDK 0.11.1 has no corresponding feature flag. This means elicitation in Zeph is not
SDK-gated; it requires a custom implementation or will need to align with SDK 0.12.x's
elicitation support. This is NOT a simple "enable a feature flag" task.

**Known issue**: `elicitation_timeout_secs` is hardcoded to 120s at multiple sites in
`mcp_bridge.rs` (lines 80, 97, 116). This should be read from `_meta` or config.
See GitHub issue #4446.

**Broader hardcoding concern**: `terminal.rs` contains 10+ call sites with hardcoded 120s
shell execution timeout (`AcpShellExecutor::new(..., 120)`). This is separate from the
elicitation timeout but indicates a systemic hardcoding pattern in `zeph-acp` that should
be addressed when elicitation is implemented — expose a `[acp.timeouts]` config section.

**Open questions**:
- What data structures does ACP elicitation use? (JSON Schema form definitions, auth challenges, preference forms)
- How does elicitation flow through TUI vs CLI vs Telegram channels?
- Does the IDE client render elicitation forms, or does Zeph render them in the terminal?
- What is the protocol for elicitation cancellation or timeout?

**Acceptance criteria** (for implementation):
- Elicitation works across session, tool call, and request scopes
- `elicitation_timeout_secs` is configurable via `[acp]` config section, not hardcoded
- Shell execution timeouts are configurable via `[acp.timeouts]` config section
- Elicitation integrates with TUI status spinner (user sees "Waiting for input…")
- Elicitation failures (timeout, cancel) propagate cleanly — no session corruption

---

### MCP-over-ACP

**Status: unstable, tracking-only**

Schema 0.13.0 (PR #1185, #1173) introduced MCP servers communicating over ACP channels as a
new transport type. SDK 0.12.0 added `agent-client-protocol-rmcp` for MCP-over-ACP proxy.

In SDK 0.12.0, `McpAcpTransport` was **removed** and replaced by advertising MCP capabilities
via `mcpCapabilities.acp` in `InitializeResponse`. Zeph does not use `McpAcpTransport` (confirmed
by grep — zero hits). No immediate action required.

**Current Zeph state**: Zeph has MCP passthrough (IDE client → Zeph → MCP server) but not the
new ACP-channel-based MCP transport (MCP servers communicating over the ACP channel itself).

**No action needed now**. Track stabilization of `agent-client-protocol-rmcp`. Evaluate when
the feature reaches stable status in the SDK.

---

### Session Usage (Token/Cost Reporting)

**Status: not-implemented**

Schema 0.10.8 (PR #454) introduced session usage messages for token consumption and context
window tracking.

**Direction clarification**: Zeph is the **agent** (server side), not the client. The correct
implementation direction is: Zeph **reports** usage TO the IDE client. Zeph consuming
ACP-reported usage from an upstream source is not applicable here.

Zeph already tracks token usage and costs internally in `zeph-core` metrics. The implementation
work is wiring this existing data to ACP session usage protocol messages.

**Protocol messages** (unstable):
- `session/usage` notification: agent → client, reports `{ prompt_tokens, completion_tokens, total_tokens, context_window_used, context_window_total }`

**Open questions**:
- Does ACP session usage include cost estimates, or token counts only?
- Is usage reported per-turn or as a cumulative session total?
- Does SDK 0.12.1 expose a typed `SessionUsage` struct, or is it raw JSON?

**Acceptance criteria** (for implementation):
- Zeph emits `session/usage` after each LLM round-trip
- Usage data comes from existing `zeph-core` cost tracker — no duplicate tracking
- `[cost]` config section in ACP mode changes from **Ignored** to **Active** in the Config Coverage table

---

### Session Delete

**Status: not-implemented (migration path documented)**

Schema 0.13.1 (PR #1216, SDK 0.12.0 PR #165) introduced `session/delete` as an unstable
standard method for removing sessions from `session/list`.

**Current Zeph state**: Zeph implements a custom `_session/delete` extension (in `custom.rs:131`).
The `_` prefix on custom extensions is now required by schema 0.12.0 (PR #883 — empty extensions
without `_` prefix are rejected).

**Migration path**:
1. Keep `_session/delete` working for existing clients
2. Add standard `session/delete` handler (behind `unstable-session-delete` feature flag)
3. When `session/delete` stabilizes upstream, remove `_session/delete` and update clients
4. Document in CHANGELOG.md as a breaking change for ACP clients

**No immediate action** — custom extension works. Migrate when standard method stabilizes.

---

## Breaking Changes Resolution (SDK 0.11.1 → 0.12.1)

| Breaking Change | Impact on Zeph | Status |
|----------------|---------------|--------|
| `McpAcpTransport` struct removed | Zeph does not use `McpAcpTransport` (grep confirmed) | **Resolved — no action** |
| `McpConnectRequest.acp_url` renamed to `acp_id` | Zeph does not use `acp_url` (grep confirmed) | **Resolved — no action** |
| `tokio` re-export removed from SDK | Zeph uses its own `tokio` dependency — does not import tokio types from the SDK (grep confirmed) | **Resolved — no action** |
| `session/close` and `session/resume` stabilized | Zeph uses feature flags `unstable-session-close` and `unstable-session-resume` — remove after SDK upgrade | **Pending SDK upgrade** |
| `_` prefix required for extension methods | Zeph's custom extension is already `_session/delete` | **Resolved — compliant** |

---

## Implementation Gap Tracker

| # | Feature | Current State | Target | Priority |
|---|---------|--------------|--------|----------|
| I1 | SDK upgrade 0.11.1 → 0.12.1 | Pinned at 0.11.1 | 0.12.1 | P1 |
| I2 | `session/resume` stable API | Uses `unstable-session-resume` flag | Remove flag, use stable API | P2 (free with I1) |
| I3 | `session/delete` migration | Custom `_session/delete` | Standard `session/delete` (unstable) | P3 |
| I4 | Providers API | Not implemented | Implement after design analysis | P2 |
| I5 | Elicitation protocol | Local empty feature flag; not SDK-gated | Full implementation after design | P2 |
| I6 | MCP-over-ACP transport | MCP passthrough only | Track stabilization | P3 |
| I7 | Session usage reporting | Internal cost tracking exists | Wire to ACP protocol messages | P3 |
| I8 | `elicitation_timeout_secs` hardcoded | 120s hardcoded (#4446) | Read from config | P3 |
| I9 | Shell timeout hardcoded | 10+ sites in `terminal.rs` with 120s | `[acp.timeouts]` config section | P3 |
| I10 | Logout method | `handlers/logout.rs` exists | Verify against upstream Preview RFD | P3 |
| I11 | Agent telemetry export | Local tracing only | Follow upstream RFD (not yet in schema) | P4 |

---

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
| `[cost]` | **Ignored** | Cost tracking not applied; will change to **Active** when Session Usage (I7) is implemented |
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
- Extension methods must start with `_` (schema 0.12.0) — bare extension names are rejected by the protocol
- Protocol version in `/agent.json` must match the compiled `agent-client-protocol` crate version

---

## Future / v2 Tracking

**Status: tracking**

Upstream has scaffolded a v2 schema module (schema 0.13.0, PR #1099) behind a separate feature
flag. The v2 proposal includes breaking changes that will require Zeph adaptation when stabilized:

| v2 Change | Expected Impact |
|-----------|----------------|
| New prompt lifecycle | Session init / turn structure changes |
| Message IDs (fork from specified IDs) | `message_ids_enabled` logic may change |
| Remote transports (streamable HTTP, WebSocket) | New transport implementations needed |
| Capabilities cleanup | Capability advertisement format changes |
| Enum variant extension (`_` prefix) | Already compliant (extension methods use `_`) |
| Streaming/non-streaming consistency | SSE/WebSocket streaming normalization |
| Session modes removal → config options | `[acp]` config section changes |
| Subagent support | Zeph subagent spawning may integrate with ACP subagent API |

**In pipeline (RFDs, not yet in schema)**:
- Agent telemetry export
- Proxy chains
- Next-edit suggestions
- Diff-delete
- Meta-propagation
- Request cancellation

No action needed now. Monitor upstream v2 progress at https://github.com/agentclientprotocol/rust-sdk.

---

## Addendum: Interop Protocol Gap Analysis (2026-04-17, updated 2026-05-19)

Cross-reference: `specs/045-interop-protocol-gaps/spec.md`

### ACP Baseline vs. arXiv:2505.02279 Survey

Zeph's ACP implementation is currently based on `agent-client-protocol = "0.11.1"` (workspace
`Cargo.toml`). Current upstream: SDK **0.12.1** / schema **0.13.2** (2026-05-17).

The survey (arXiv:2505.02279) describes ACP's capability advertisement and re-negotiation
model as a differentiating feature vs. MCP and A2A.

**Capability re-negotiation status: Unverified.** The `agent-client-protocol` 0.11 SDK
includes capability fields in the session handshake message. Dynamic re-negotiation during
an active session has not been confirmed tested in Zeph's `AcpSessionManager`.

This does not block any current feature. It is tracked as a P3 follow-up in
`specs/045-interop-protocol-gaps/spec.md` under "P3 Follow-up: ACP capability re-negotiation
integration test".

### Version Upgrade Note

To upgrade `agent-client-protocol` from 0.11.1 to 0.12.1:
1. Review breaking changes in the SDK changelog (summarized in Breaking Changes Resolution table above).
2. Update `Cargo.toml` workspace dependency: `agent-client-protocol = "0.12.1"`, `agent-client-protocol-tokio = "0.12.1"`.
3. Remove `unstable-session-resume` and `unstable-session-close` feature flags from `crates/zeph-acp/Cargo.toml` and root `Cargo.toml`.
4. Run `cargo nextest run --workspace --features full --lib --bins` — ACP session tests must pass.
5. Verify no tokio type imports from `agent_client_protocol` or `agent_client_protocol_tokio`.
6. Update the capability matrix in `specs/045-interop-protocol-gaps/spec.md` accordingly.
7. Update `/agent.json` `protocol` field from `"acp/0.12.0"` to `"acp/0.13.2"`.
