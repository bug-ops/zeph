# Spec: A2A Protocol

## Sources

### External
- A2A specification (v0.2.1): https://raw.githubusercontent.com/a2aproject/A2A/main/docs/specification.md
- A2A project: https://github.com/a2aproject/A2A

### Internal
| File | Contents |
|---|---|
| `crates/zeph-a2a/src/types.rs` | `Task`, `Message`, `AgentCard`, `Artifact` |
| `crates/zeph-a2a/src/jsonrpc.rs` | JSON-RPC 2.0 envelope, error codes |
| `crates/zeph-a2a/src/client.rs` | `A2aClient`, `send_message`, `stream_message`, `get_task`, `cancel_task` |
| `crates/zeph-a2a/src/discovery.rs` | `AgentRegistry`, TTL cache, `/.well-known/agent.json` |
| `crates/zeph-a2a/src/card.rs` | `AgentCard` serialization |
| `crates/zeph-a2a/src/server/mod.rs` | `A2aServer`, `TaskProcessor` trait |
| `crates/zeph-a2a/src/server/handlers.rs` | JSON-RPC method handlers |
| `crates/zeph-a2a/src/server/state.rs` | `TaskManager`, in-memory task store |
| `crates/zeph-a2a/src/error.rs` | `A2aError` with JSON-RPC error codes |

---

`crates/zeph-a2a/` (feature: `a2a`) — A2A protocol v0.2.1, JSON-RPC 2.0.

## Roles

- **Client**: Zeph connects to another A2A-compatible agent and delegates tasks
- **Server**: Zeph exposes an A2A endpoint for other agents to call (`zeph-a2a?/server`)

## Agent Discovery

```
AgentRegistry
├── cache: RwLock<HashMap<String, CachedCard>>  — URL → AgentCard, TTL-cached
└── discovery: GET {base_url}/.well-known/agent.json → AgentCard
```

- Discovery endpoint: `/.well-known/agent.json` — standard A2A well-known path
- `AgentCard`: describes capabilities, supported methods, authentication requirements
- Cache TTL: configurable; prevents repeated discovery requests to the same agent

## JSON-RPC 2.0 Protocol

```
Request:  { "jsonrpc": "2.0", "id": "...", "method": "tasks/send", "params": {...} }
Response: { "jsonrpc": "2.0", "id": "...", "result": {...} }
Error:    { "jsonrpc": "2.0", "id": "...", "error": { "code": N, "message": "..." } }
```

- All A2A methods follow JSON-RPC 2.0 — no custom envelopes
- `id` field must be echoed back in response — required for request/response correlation
- Error codes follow JSON-RPC standard ranges + A2A-defined application codes

## Core Methods

| Method | Direction | Description |
|---|---|---|
| `message/send` | Client → Agent | Submit task (request-response), returns Task with initial status |
| `message/stream` | Client → Agent | Submit task (SSE streaming), returns TaskEventStream |
| `tasks/get` | Client → Agent | Fetch task by ID, optional `history_length` truncation |
| `tasks/cancel` | Client → Agent | Move task to `Canceled` — fails with `-32002` if already terminal |

Error codes: `-32001` (task not found), `-32002` (task not cancelable), standard `-32600`/`-32603` for protocol errors.

## Task Lifecycle

```
submitted → working → (input-required) → completed
                    → (input-required) → working → ...
                    → failed | canceled | rejected | auth-required | unknown
```

Terminal states: `completed | failed | canceled | rejected`

- `state` enum: `submitted | working | input-required | completed | failed | canceled | rejected | auth-required | unknown`
- `status.timestamp`: RFC3339 — cross-timezone compatible
- SSE streaming events: `{kind: "status-update" | "artifact-update", taskId, ..., final: bool}`
- SSE completion signaled by `[DONE]` marker or stream close
- **History is append-only** — never reorder or delete message history entries
- **Artifacts are immutable** once created — no updates, only append
- Task IDs: UUID v4; Context IDs optional but persistent through session

## Key Invariants

- `/.well-known/agent.json` must be served for agent discovery — cannot be disabled
- All responses must include `"jsonrpc": "2.0"` and echo the request `id`
- `AgentCard` must accurately reflect supported capabilities — no undeclared methods
- `cancel` fails with `-32002` if task is in a terminal state — never silently succeed
- History is append-only — never reorder or delete entries
- Artifacts are immutable once created — no in-place updates
- SSE stream must emit `[DONE]` on completion — clients depend on this terminator
- SSRF protection: DNS lookup + IP check post-fetch (prevents DNS rebinding attacks)
- TLS enforcement: if `require_tls` enabled, `http://` URLs must be rejected
- Server feature (`zeph-a2a?/server`) is independent of client — can run one without the other
