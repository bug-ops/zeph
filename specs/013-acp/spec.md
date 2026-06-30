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
updated: 2026-06-30
status: approved
related:
  - "[[MOC-specs]]"
  - "[[014-a2a/spec]]"
---

# Spec: ACP (Agent Client Protocol)

> [!info]
> ACP transports, session management, permissions, fork/resume,
> capability advertisement, agent-client-protocol 1.0.1 / schema =1.1.0 compatibility.

## Spec Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-04-08 | sdd | Initial spec (SDK 0.11.1 / schema 0.12.0) |
| 1.1 | 2026-05-19 | sdd | Updated to SDK 0.12.1 / schema 0.13.2; added Providers API, Elicitation, MCP-over-ACP, Session Usage, Session Delete migration, v2 tracking, breaking changes resolution |
| 1.2 | 2026-05-29 | sdd | Mark Providers API, Elicitation protocol, Session Usage, and session/delete as implemented; update SDK to 0.12.1; wire IDE-provided MCP servers into do_new_session; add blocking-await timeout note |
| 1.3 | 2026-06-06 | sdd | ACP 0.14.0 protocol bump: bumped core 0.12.1→0.14.0, schema pinned =0.13.6; removed session/set_model RPC (model switching preserved via set_config_option); removed inbound message-id echo feature; renamed provider ext-method types to singular; stabilized delete/logout/resume/add-dirs feature flags; renamed session-usage upstream gate; added elicitation core passthrough; documented MessageId newtype change |
| 1.4 | 2026-06-30 | developer | ACP 1.0.1 schema-path migration: bumped core 0.14.0→1.0.1, schema pinned =1.1.0; mechanical `schema::X` → `schema::v1::X` reorg (root re-export removed upstream), `ProtocolVersion`/`MaybeUndefined`/`IntoOption`/`IntoMaybeUndefined` stay flat; removed root re-exports `cookbook`/`handler`/`jsonrpcmsg`/six message enums; deleted 5 long-dead `#[cfg(any())]` test modules (153 unverifiable sites); no handler/transport/builder logic changed; `unstable_cancel_request` and `model_config` evaluated and deferred to follow-up issues |

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

### Agent Spawner Contract (1.0.1)

Agent sessions use the `Agent.builder()` / `run_agent()` pattern. Session state is `Arc`-wrapped.
Session tasks are launched via `tokio::task::spawn_local` inside a `LocalSet` — the
`AgentSpawner` closure returns `Pin<Box<dyn Future<Output = ()> + 'static>>` (`!Send`).

SDK 0.12.0 removed `McpAcpTransport` and the direct `tokio` re-export; the dead
`agent-client-protocol-tokio` crate was also removed entirely in the 0.14.0 bump.
Zeph is unaffected: `McpAcpTransport` was never used, Zeph has its own `tokio` dependency,
and `agent-client-protocol-tokio` was removed from both workspace `Cargo.toml` and `crates/zeph-acp/Cargo.toml`.

`session/close`, `session/resume`, `session/delete`, and `session/logout` are unconditional in
core 1.0.1 (unconditional since the 0.14.0 bump; unaffected by the 1.0.1 schema-path migration).
The corresponding `unstable-session-*` Zeph feature flags are tombstoned as
no-op `= []` (retained only so root `Cargo.toml` forwarding resolves without changes).

**Status: implemented** (SDK upgraded to 0.14.0 / schema =0.13.6; schema-path migrated to 1.0.1 / =1.1.0 in this PR)

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
- Model switching: client requests a specific model via `session/set_config_option` with `config_id="model"` (see Model Switching below)
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
| `additional_directories` | `Vec<String>` | `[]` | **Request-side allowlist.** Paths a client may pass in `sessionInit.additionalDirectories`. Paths not in this list are rejected at session start. This is NOT a protocol advertisement — it is a server-side gate. Field is unconditional (degated in the 0.14.0 bump; unaffected by the 1.0.1 schema-path migration). |
| `auth_methods` | `Vec<String>` | `["agent"]` | Accepted authentication methods. MVP: only `"agent"` is valid. Unknown values are rejected at deserialization. |

> **Changed in 0.14.0 bump**: `message_ids_enabled` is retained as a no-op field for config-schema
> compatibility (read by `acp_commands.rs`). The `PromptRequest.message_id` and
> `PromptResponse.user_message_id` protocol fields were deleted upstream in schema 0.13.6; the
> inbound message-id echo behaviour is removed.

### Key Invariants

- `additional_directories` is a **request-side allowlist**: paths requested by the client must be
  a prefix of a configured allowed path; requests with non-allowed paths are rejected with
  `AcpError::PermissionDenied` at session start — never silently ignored
- `auth_methods` must only contain `"agent"` for MVP; unknown variants cause a hard deserialization
  error at startup to prevent misconfigured deployments from silently accepting unexpected auth

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

**Status: stable** (stabilized in schema 0.12.2, SDK 0.12.0; unconditional in core 1.0.1 (since the 0.14.0 bump))

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

**Status: stable** (stabilized in schema 0.12.2, SDK 0.12.0; unconditional in core 1.0.1 (since the 0.14.0 bump))

Reconnect to an existing session by ID, restoring conversation history and tool context.
Previously gated behind `unstable-session-resume` feature flag in Zeph.

The `unstable-session-resume` Zeph feature flag is now a tombstone `= []`. All `#[cfg(feature =
"unstable-session-resume")]` gates are removed; the resume handler runs unconditionally.

### session/delete

**Status: stable** (unconditional in core 1.0.1 (since the 0.14.0 bump))

Remove a session from the `session/list` registry. Previously gated behind `unstable-session-delete`.
The `unstable-session-delete` Zeph feature flag is now a tombstone `= []`. All cfg gates removed.

Custom `_session/delete` extension (backward compat) is retained alongside the standard method.

### session/logout

**Status: stable** (unconditional in core 1.0.1 (since the 0.14.0 bump))

Previously gated behind `unstable-logout`. The `unstable-logout` Zeph feature flag is now a
tombstone `= []`. All cfg gates removed; logout handler runs unconditionally.

### Capability Negotiation

**Status: stable**

ACP server advertises its capabilities in the `initialize` response and via the `/agent.json` endpoint.

#### /agent.json Endpoint

`GET /agent.json` returns a JSON document describing the agent's identity, declared capabilities, supported protocol version, and authentication methods. This endpoint is unauthenticated and used by IDE clients for discovery.

```json
{
  "name": "...",
  "version": "...",
  "protocol": "acp",
  "protocol_version": 1,
  "transports": { "http_sse": { "url": "/acp" }, "websocket": { "url": "/acp/ws" }, "health": { "url": "/health" } },
  "authentication": { "type": "bearer" }
}
```

#### Protocol Version

Zeph uses `agent-client-protocol 1.0.1` / `schema =1.1.0`.
The `/agent.json` (`transport/discovery.rs`) document emits a fixed `"protocol": "acp"` string plus a
separate numeric `"protocol_version": acp::schema::ProtocolVersion::LATEST` field (`ProtocolVersion`
stays flat at the crate root — not relocated under `schema::v1::` by the 1.0.1 migration). `LATEST == V1`
is unchanged across 0.14.0 → 1.0.1 / 0.13.6 → 1.1.0, so this wire output does not change with the
schema crate version. The example above previously and incorrectly described the wire output as
`"protocol": "acp/<schema-version>"` — that never matched the implementation; this entry corrects the
spec to match `discovery.rs`, not vice versa.

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

## Feature Flags

| Flag | Status | Notes |
|------|--------|-------|
| `unstable-session-fork` | **active** | Still gated upstream (`unstable_session_fork`) |
| `unstable-session-usage` | **active** | Gate renamed upstream: now forwards `agent-client-protocol/unstable_end_turn_token_usage` (was `unstable_session_usage`). `Usage` struct + `PromptResponse.usage` field are ALL gated — not unconditional. |
| `unstable-elicitation` | **active** | Now also adds `agent-client-protocol/unstable_elicitation` passthrough so core wires `elicitation/create` |
| `unstable-llm-providers` | **active** | Still gated upstream (`unstable_llm_providers`); provider type renames apply here (see Providers API) |
| `unstable-auth-methods` | **active** | Still gated upstream (`unstable_auth_methods`) |
| `unstable-boolean-config` | **active** | Still gated upstream (`unstable_boolean_config`) |
| `unstable-session-delete` | **tombstone** `= []` | Stabilized — `session/delete` handler is unconditional in core 1.0.1 (since the 0.14.0 bump). Flag retained as no-op for workspace forwarding (root `Cargo.toml` references it). |
| `unstable-session-resume` | **tombstone** `= []` | Stabilized — `session/resume` handler is unconditional in core 1.0.1 (since the 0.14.0 bump). Flag retained as no-op. |
| `unstable-logout` | **tombstone** `= []` | Stabilized — logout handler is unconditional in core 1.0.1 (since the 0.14.0 bump). Flag retained as no-op. |
| `unstable-session-add-dirs` | **tombstone** `= []` | Stabilized — `additional_directories` field is plain `Vec<PathBuf>`, unconditional since schema 0.13.6 (now schema 1.1.0). Flag retained as no-op. |
| `unstable-message-id` | **tombstone** `= []` | Removed — `PromptRequest.message_id` and `PromptResponse.user_message_id` deleted upstream. Entire inbound echo feature removed. Flag retained as no-op for workspace forwarding. |
| `unstable_cancel_request` | **not adopted** | Available at the ACP-crate level since SDK 0.15.1 (predates Zeph's 0.14.0 baseline; not "new in 1.0.1" relative to upstream, only relative to Zeph's prior pin). Exposes `RequestCancellation` + `is_cancel_request_notification`. Zeph does **not** define an `unstable-cancel-request` Cargo feature and does **not** wire a `$/cancel_request` handler — deferred to a follow-up issue; today cancellation is handled entirely via the internal `cancel_signal: Arc<Notify>` in `agent/handlers/cancel.rs` (`session/cancel`). |
| `unstable-session-model` | **DELETED** | Removed entirely — `session/set_model` RPC deleted upstream. Feature name removed from Cargo.toml and root `Cargo.toml`. Model switching survives via `set_config_option`. |

> **Tombstone flags** are `= []` no-ops retained solely so root `Cargo.toml` feature forwarding
> resolves without changes. They add zero behavior.

---

## Model Switching

**Status: preserved via stable mechanism**

The dedicated `session/set_model` RPC method was removed upstream (deleted in `agent-client-protocol`
0.14.0 / schema 0.13.6). This is NOT a capability loss.

Model switching is FULLY preserved via two stable paths:

1. **`session/set_config_option`** with `config_id="model"` and `value=<model-name>` — the
   canonical stable path. Runs identical logic to the former `session/set_model`: calls
   `provider_factory(value)`, validates against `available_models_snapshot()`, updates
   `provider_override`, and emits `SessionInfoUpdate` with `model_meta`.
2. **`$/model` slash command** — IDE/CLI convenience; internally dispatches to the same
   `apply_session_config` path.

`session/set_mode` (behavioral persona switch: `code`/`architect`/`ask`) is an orthogonal
concept, NOT a replacement for model switching. Mode and model are independent.

> **NEVER** describe the removal of `session/set_model` as a capability loss. Model switching
> survives unconditionally via `session/set_config_option`.

---

## Message ID Echo (REMOVED)

**Status: removed in 0.14.0 bump**

`PromptRequest.message_id` and `PromptResponse.user_message_id` were deleted upstream in
schema 0.13.6. The entire inbound message-id echo feature is removed from Zeph:

- `message_ids_enabled` config field retained as no-op (config-schema compatibility)
- `current_message_id` session slot removed
- `build_prompt_response` no longer accepts or echoes a message ID
- `apply_message_id_to_chunk` removed (no live data source)
- `unstable-message-id` feature is a tombstone `= []`

`ContentChunk.message_id` field still exists in schema 0.13.6 for potential future
agent-generated per-chunk IDs, but Zeph does not inject it (no inbound source).

### MessageId Type

In schema 0.13.6, `MessageId` is a newtype: `MessageId(pub Arc<str>)`. The chunk builder
accepts `impl IntoOption<MessageId>`, where `IntoOption<MessageId>` is implemented for
`&str` **only** (not `String`). Passing `String` will not compile — always pass `&str`.

---

## New Protocol Features

### Providers API

**Status: implemented** (commit #4473, PR #4473)

Schema 0.11.7 introduced a providers management API (`unstable` in SDK):

| Method | Description |
|--------|-------------|
| `providers/list` | Returns available LLM providers for the session |
| `providers/set` | Sets the active provider for the session |
| `providers/disable` | Disables a provider for the session |

**Breaking change in 0.14.0 bump — type renames (singular):**

| Old type name | New type name |
|---------------|---------------|
| `SetProvidersRequest` | `SetProviderRequest` |
| `SetProvidersResponse` | `SetProviderResponse` |
| `DisableProvidersRequest` | `DisableProviderRequest` |
| `DisableProvidersResponse` | `DisableProviderResponse` |

All renamed types have `::new()` constructors. All four remain gated behind
`unstable_llm_providers` (Zeph flag `unstable-llm-providers` retained).

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

**Status: implemented** (commit #4473, PR #4473; `elicitation_timeout_secs` wired from `_meta` in `mcp_bridge.rs` — commit #4453; `elicitation_enabled` read from `_meta` — commit #4441)

Schema 0.11.5 introduced structured user input (elicitation) across three scopes:
- **Session scope** (0.11.5, PR #792): agent requests structured input during session initialization
- **Tool call scope** (0.11.5, PR #769): agent requests structured input before executing a tool
- **Request scope** (0.11.5, PR #771): agent requests structured input during prompt processing
- **Scoped by mode** (0.11.6, PR #966): elicitation behavior varies by mode

**Current Zeph state**: `unstable-elicitation` in `crates/zeph-acp/Cargo.toml` now includes
`agent-client-protocol/unstable_elicitation` passthrough (added in the 0.14.0 bump). This wires
core's `elicitation/create` request dispatch path. Zeph already implements elicitation in
`elicitation.rs`; the core passthrough ensures `elicitation/create` is registered.

**Fixed**: `elicitation_timeout_secs` is now read from `_meta` in `mcp_bridge.rs` (commit #4453).
`elicitation_enabled` is read from `_meta` rather than being hardcoded to `false` (commit #4441).

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

**Status: implemented** (commit #4522, PR #4522; `session/usage` wired from `zeph-core` cost tracker)

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

**Status: implemented** (commit #4464; standard `session/delete` handler added; `_session/delete` retained for backward compatibility)

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
| `session/close` and `session/resume` stabilized | Feature flags removed; handlers unconditional | **Resolved** |
| `_` prefix required for extension methods | Zeph's custom extension is already `_session/delete` | **Resolved — compliant** |

## Breaking Changes Resolution (SDK 0.12.1 → 0.14.0)

| Breaking Change | Impact on Zeph | Status |
|----------------|---------------|--------|
| `agent-client-protocol` bumped to `0.14.0`, schema pinned `=0.13.6` | Workspace `Cargo.toml` updated; `=` pin required for schema | **Resolved** |
| `agent-client-protocol-tokio` dead dep removed | Dep line deleted from workspace + crate `Cargo.toml` | **Resolved** |
| `session/set_model` RPC deleted upstream | Handler + file + tests deleted; model switching preserved via `session/set_config_option` (config_id="model") | **Resolved** |
| `PromptRequest.message_id` removed upstream | Entire inbound message-id echo feature removed; `unstable-message-id` tombstoned | **Resolved** |
| `PromptResponse.user_message_id` removed upstream | Removed from `build_prompt_response`; was a hard compile break | **Resolved** |
| `SetProvidersRequest/Response` → `SetProviderRequest/Response` (singular) | Renamed at all ext-method dispatch sites | **Resolved** |
| `DisableProvidersRequest/Response` → `DisableProviderRequest/Response` (singular) | Renamed at all ext-method dispatch sites | **Resolved** |
| `unstable_session_usage` gate renamed to `unstable_end_turn_token_usage` | `unstable-session-usage` feature re-pointed; `Usage` struct + `PromptResponse.usage` still gated | **Resolved** |
| `unstable_elicitation` added to core 0.14.0 | `unstable-elicitation` feature now passes through to core | **Resolved** |
| `MessageId` type changed to newtype `MessageId(pub Arc<str>)` | `IntoOption<MessageId>` impl for `&str` only — no `String` | **Resolved** |
| `session/delete`, `session/resume`, `session/logout`, `additional_directories` stabilized | Feature flags tombstoned `= []`; all cfg gates removed | **Resolved** |

## Breaking Changes Resolution (SDK 0.14.0 → 1.0.1)

| Breaking Change | Impact on Zeph | Status |
|----------------|---------------|--------|
| `agent-client-protocol` bumped to `1.0.1`, schema pinned `=1.1.0` | Workspace `Cargo.toml` updated; `=` pin required for schema (unchanged convention) | **Resolved** |
| Schema crate `1.1.0` removed the flat `pub use v1::*` root re-export (schema types now live only under `schema::v1::`); ACP crate `1.0.1` mirrors this in `schema/mod.rs` | Mechanical `acp::schema::X` → `acp::schema::v1::X` reorg across `crates/zeph-acp/src/**` and `crates/zeph-acp/tests/**` (~506 live sites); `ProtocolVersion`, `MaybeUndefined`, `IntoOption`, `IntoMaybeUndefined` stay flat at crate root — explicitly excluded from the reorg | **Resolved** |
| Root re-exports `cookbook`, `handler`, `jsonrpcmsg`, and the six root enum re-exports (`AgentRequest`/`AgentResponse`/`AgentNotification`/`ClientRequest`/`ClientResponse`/`ClientNotification`) removed from the ACP crate root | Zeph used none of `cookbook`/`handler`/`jsonrpcmsg`; the only root-enum use site (`tests/integration.rs` `acp::ClientRequest::ExtMethodRequest`) repointed to `acp::schema::v1::ClientRequest::ExtMethodRequest` | **Resolved** |
| `Builder`/`ConnectionTo`/`Dispatch`/`Responder`/`ByteStreams`/`on_receive_request!`/`on_receive_dispatch!` builder API | Byte-identical between 0.14.0 and 1.0.1 for the methods Zeph uses — no handler, transport, or builder-chain code changed shape | **Resolved — no action** |
| Feature flags: ACP crate `[features]` add only `unstable_cancel_request`; schema `[features]` unchanged | No renames affecting Zeph's existing `unstable-*` feature mappings; `unstable_cancel_request` evaluated and deferred (#5362), not adopted in this PR | **Resolved — no action** |
| `model_config` option category stabilized in schema 1.1.0 (reachable, schema 1.2.0 stabilizes `unstable_cancel_request` but is **not** reachable — ACP 1.0.1 pins schema `=1.1.0` exactly) | Evaluated and deferred to a follow-up issue (#5361) to keep this PR a clean mechanical bump | **Deferred — not capability loss** |
| 5 long-dead `#[cfg(any())]` test modules (153 of 616 `acp::schema::` src sites, pre-dating ACP 0.11) were unreachable by any feature toggle and contained stale pre-0.14.0 root-path references that didn't even compile | Deleted entirely: `terminal.rs`, `custom.rs`, `fs.rs`, `mcp_bridge.rs` (inline dead `mod tests`), `agent/mod.rs` + external `agent/tests.rs` (dead `mod tests;` declaration) — removes false-green risk where a sed-rewritten but type-unchecked block would silently mask path errors | **Resolved** |

---

## Implementation Gap Tracker

| # | Feature | Current State | Target | Priority |
|---|---------|--------------|--------|----------|
| I1 | SDK upgrade 0.11.1 → 0.12.1 | **Implemented** (#4464) | ✓ Done | — |
| I2 | `session/resume` stable API | **Implemented** — feature flags removed | ✓ Done | — |
| I3 | `session/delete` migration | **Implemented** — standard handler added (#4464) | Deprecate `_session/delete` when clients migrate | P4 |
| I4 | Providers API | **Implemented** (#4473) | ✓ Done | — |
| I5 | Elicitation protocol | **Implemented** (#4473, #4453, #4441) | ✓ Done | — |
| I6 | MCP-over-ACP transport | MCP passthrough only | Track stabilization | P3 |
| I7 | Session usage reporting | **Implemented** (#4522) | ✓ Done | — |
| I8 | `elicitation_timeout_secs` hardcoded | **Fixed** — read from `_meta` (#4453) | ✓ Done | — |
| I9 | Shell timeout hardcoded | 10+ sites in `terminal.rs` with 120s | `[acp.timeouts]` config section | P3 |
| I10 | Logout method | **Stable** — degated in 0.14.0 bump | ✓ Done | — |
| I11 | Agent telemetry export | Local tracing only | Follow upstream RFD (not yet in schema) | P4 |
| I12 | IDE-provided MCP servers | **Implemented** — wired into `do_new_session` (#4444) | ✓ Done | — |
| I13 | Blocking awaits in handlers | **Fixed** — bounded with configurable timeouts (#4538) | ✓ Done | — |
| I14 | SDK upgrade 0.12.1 → 0.14.0 | **Implemented** | ✓ Done | — |
| I15 | Remove `session/set_model` handler | **Implemented** | ✓ Done | — |
| I16 | Remove inbound message-id echo | **Implemented** | ✓ Done | — |
| I17 | Provider type renames (singular) | **Implemented** | ✓ Done | — |
| I18 | Re-point `unstable-session-usage` gate | **Implemented** | ✓ Done | — |
| I19 | Add elicitation core passthrough | **Implemented** | ✓ Done | — |
| I20 | SDK upgrade 0.14.0 → 1.0.1 (schema-path reorg) | **Implemented** (this PR) | ✓ Done | — |
| I21 | Adopt `model_config` option category | Deferred — schema 1.1.0 stabilizes it but Zeph does not expose it | Follow-up issue #5361 | P3 |
| I22 | Wire `unstable_cancel_request` ($/cancel_request handler) | Deferred — feature flag not added, no handler wired | Follow-up issue #5362 | P3 |

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
flag. `unstable_protocol_v2` is still experimental at schema 1.1.0 (and remains so at schema
1.2.0, the next version up — not reachable under Zeph's `=1.1.0` pin); `Client::v2()`/`Agent::v2()`
exist upstream but stay behind the gate. The v2 proposal includes breaking changes that will
require Zeph adaptation when stabilized:

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

**Deferred unstable surfaces (evaluated during the 1.0.1 migration, not adopted)**:
- `unstable_mcp_over_acp` — MCP-over-ACP transport; Zeph keeps the existing passthrough bridge
  (`mcp_bridge.rs`) instead, see "MCP-over-ACP" above. No reachability blocker, deferred on scope.
- `unstable_nes` (next-edit suggestions) — no Zeph use case identified yet; revisit if an IDE
  client requests it.
- `unstable_plan_operations` — distinct from the **stable** `acp::schema::v1::PlanEntryStatus`
  (3 live sites, unrelated and unaffected by this deferral); plan-operation RPCs themselves are
  not wired in Zeph.

**In pipeline (RFDs, not yet in schema)**:
- Agent telemetry export
- Proxy chains
- Next-edit suggestions
- Diff-delete
- Meta-propagation

`unstable_cancel_request` has graduated out of this list — it is implemented (not just RFD) at
the ACP-crate level since SDK 0.15.1, see "Feature Flags" above; Zeph has evaluated and deferred
adoption (#5362), it is not blocked on upstream availability.

No action needed now. Monitor upstream v2 progress at https://github.com/agentclientprotocol/rust-sdk.

---

## Addendum: Interop Protocol Gap Analysis (2026-04-17, updated 2026-06-30)

Cross-reference: `specs/045-interop-protocol-gaps/spec.md`

### ACP Baseline vs. arXiv:2505.02279 Survey

Zeph's ACP implementation is based on `agent-client-protocol = "1.0.1"` / schema `=1.1.0`
(workspace `Cargo.toml`, updated in this PR).

The survey (arXiv:2505.02279) describes ACP's capability advertisement and re-negotiation
model as a differentiating feature vs. MCP and A2A.

**Capability re-negotiation status: Unverified.** Dynamic re-negotiation during an active
session has not been confirmed tested in Zeph's `AcpSessionManager`.

This does not block any current feature. It is tracked as a P3 follow-up in
`specs/045-interop-protocol-gaps/spec.md` under "P3 Follow-up: ACP capability re-negotiation
integration test".

### Version Upgrade Note (0.12.1 → 0.14.0, completed in this PR)

1. Review Breaking Changes Resolution table (SDK 0.12.1 → 0.14.0) above.
2. Workspace: `agent-client-protocol = "0.14.0"`, `agent-client-protocol-schema = "=0.13.6"`; delete `agent-client-protocol-tokio`.
3. Crate `Cargo.toml`: tombstone degated features as `= []`; fix `unstable-session-usage` → `["agent-client-protocol/unstable_end_turn_token_usage"]`; add core passthrough to `unstable-elicitation`.
4. Delete `handlers/set_session_model.rs`; remove all `session/set_model` handler code and tests.
5. Remove all inbound message-id plumbing; `message_ids_enabled` config field retained as no-op.
6. Rename `SetProvidersRequest/Response` and `DisableProvidersRequest/Response` to singular.
7. Degate all cfg sites for delete/logout/resume/add-dirs.
8. Build: `cargo check -p zeph-acp --features full`; `cargo nextest run -p zeph-acp --all-features`.
9. Live round-trip test: session/new → prompt → set_config_option{model} → set_mode → session/delete → logout.
10. Update `/agent.json` `protocol` field to `"acp/0.13.6"`.

### Version Upgrade Note (0.14.0 → 1.0.1, completed in this PR)

1. Review Breaking Changes Resolution table (SDK 0.14.0 → 1.0.1) above.
2. Workspace: `agent-client-protocol = "1.0.1"`, `agent-client-protocol-schema = "=1.1.0"`. Crate
   `Cargo.toml` `[dependencies]`/`[features]` blocks unchanged — every existing feature mapping
   still resolves against 1.0.1/1.1.0.
3. Delete the 5 long-dead `#[cfg(any())]` test modules first (`terminal.rs`, `custom.rs`, `fs.rs`,
   `mcp_bridge.rs`, `agent/mod.rs` + external `agent/tests.rs`) — before the path-reorg pass, not
   after, so every remaining edit is compiler-checked.
4. Mechanical `acp::schema::X` → `acp::schema::v1::X` reorg (and the `agent_client_protocol::schema::X`
   / `agent_client_protocol_schema::X` spellings, including nested `schema::{A, B, C}` use-blocks
   and local `use agent_client_protocol_schema as schema;` aliases) across `src/**` and `tests/**`,
   excluding `ProtocolVersion`/`MaybeUndefined`/`IntoOption`/`IntoMaybeUndefined`.
5. `tests/integration.rs`: `acp::ClientRequest::ExtMethodRequest` → `acp::schema::v1::ClientRequest::ExtMethodRequest`.
6. Do **not** add `unstable-cancel-request`, adopt `model_config`, or adopt the
   `agent-client-protocol-tokio`/`-rmcp`/`-http` helper crates — all deferred (#5361, #5362).
7. Build: `cargo +nightly fmt`; `cargo clippy --profile ci --workspace --all-targets --features
   "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`; `cargo nextest run
   --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler"
   --lib --bins`; rustdoc gate with both `RUSTFLAGS="-D warnings"` and
   `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links"`. Additionally build `zeph-acp` standalone
   under each individually-toggled unstable feature (`unstable-session-fork`, `-session-usage`,
   `-elicitation`, `-llm-providers`, `-auth-methods`, `-boolean-config`, `acp-http`) — these are
   skipped by the default-feature build and would otherwise hide path errors behind a cfg gate.
8. Live round-trip test: `cargo nextest run -p zeph-acp --all-features` exercises
   `initialize_handshake`, `prompt_round_trip_returns_end_turn`, `cancel_before_prompt_returns_cancelled`,
   and `unknown_ext_method_returns_null` in `tests/integration.rs` — no panics, no serde errors.
9. `/agent.json` `protocol` field is **unchanged** by this migration — it was never
   `"acp/<schema-version>"` in the implementation (see M2 correction in "Protocol Version" above);
   it stays `"protocol": "acp"` + numeric `"protocol_version"`.
