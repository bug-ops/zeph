---
aliases:
  - A2A Protocol
  - Agent-to-Agent
  - IBCT
tags:
  - sdd
  - spec
  - protocol
  - a2a
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[013-acp/spec]]"
  - "[[010-security/spec]]"
---

# Spec: A2A Protocol

> [!info]
> A2A protocol, agent discovery, JSON-RPC 2.0, IBCT (Invocation-Bound Capability Tokens),
> HMAC-SHA256 signatures, key_id rotation, X-Zeph-IBCT header.

## Sources

### External
- A2A specification: https://raw.githubusercontent.com/a2aproject/A2A/main/docs/specification.md
- A2A project: https://github.com/a2aproject/A2A

### Internal
| File | Contents |
|---|---|
| `crates/zeph-a2a/src/types.rs` | `Task`, `Message`, `AgentCard`, `Artifact` |
| `crates/zeph-a2a/src/jsonrpc.rs` | JSON-RPC 2.0 envelope, error codes |
| `crates/zeph-a2a/src/client.rs` | `A2aClient`, `send_message`, `stream_message`, `get_task`, `cancel_task` |
| `crates/zeph-a2a/src/discovery.rs` | `AgentRegistry`, TTL cache, `/.well-known/agent.json`, `CardTrustPolicy` enforcement |
| `crates/zeph-a2a/src/card.rs` | `AgentCard` serialization |
| `crates/zeph-a2a/src/card_signing.rs` | `AgentCardSignature` JWS verification (`card-signing` feature), `TrustedKey`, `SigAlg`, `SignatureVerification` |
| `crates/zeph-a2a/src/server/mod.rs` | `A2aServer`, `TaskProcessor` trait |
| `crates/zeph-a2a/src/server/handlers.rs` | JSON-RPC method handlers |
| `crates/zeph-a2a/src/server/state.rs` | `TaskManager`, in-memory task store |
| `crates/zeph-a2a/src/error.rs` | `A2aError` with JSON-RPC error codes |

---

`crates/zeph-a2a/` (feature: `a2a`) — Agent-to-Agent protocol, JSON-RPC 2.0.

## Roles

- **Client**: Zeph connects to another A2A-compatible agent and delegates tasks
- **Server**: Zeph exposes an A2A endpoint for other agents to call (`zeph-a2a?/server`)

## Agent Discovery

```
AgentRegistry
├── cache: RwLock<HashMap<String, CachedCard>>  — URL → AgentCard, TTL-cached
├── trust: TrustConfig { policy: CardTrustPolicy, trusted_keys: Vec<TrustedKey> }
└── discovery: GET {base_url}/.well-known/agent.json → AgentCard → check_trust() → cache
```

- Discovery endpoint: `/.well-known/agent.json` — standard A2A (pre-1.0.0) well-known path.
  A2A 1.0.0 renames this to `/.well-known/agent-card.json`; Zeph has not adopted the rename
  (see `A2A_PROTOCOL_VERSION` below) — a pure-1.0.0 peer that only serves the new path is not
  currently discoverable.
- `AgentCard`: describes capabilities, supported methods, authentication requirements, and
  (A2A 1.0.0 §4.4.7) an optional `signatures: Vec<AgentCardSignature>` — `#[serde(default)]`,
  so unsigned/pre-1.0.0 peers deserialize with an empty vec (backward-compatible).
- Cache TTL: configurable; prevents repeated discovery requests to the same agent

### Card trust policy (`CardTrustPolicy`, #5928)

`AgentRegistry::discover()` runs an optional trust check on every card it fetches, combining
two independent axes via most-severe-wins precedence (`Accept < Warn < Reject`):

- **URL-origin consistency**: the queried `base_url` vs. the card's self-declared `url` field,
  compared by scheme + host + port (RFC 6454 origin semantics, not full path).
- **Signature verification** (`crates/zeph-a2a/src/card_signing.rs`, feature `card-signing`):
  JWS signatures over the RFC 8785 JCS canonicalization of the *raw received JSON* (never a
  re-serialization of the typed `AgentCard` struct) with `signatures` removed. Only ES256
  (P-256) is supported; other algorithms resolve to `Unverifiable`. All signatures in the array
  are evaluated — verification is order-independent, so key rotation (old+new signature during
  overlap) and multi-party attestation both work correctly.

`CardTrustPolicy` is tri-state, default **`Ignore`** (byte-identical to pre-#5928 behavior):

| Policy | URL mismatch | Unverifiable/FeatureDisabled signature | Invalid (tampered) signature |
|---|---|---|---|
| `ignore` | accept | accept | accept |
| `prefer` | warn + accept | warn + accept | **reject** |
| `require` | **reject** | **reject** | **reject** |

Two independent `CardTrustPolicy` enums exist by design: `zeph_a2a::discovery::CardTrustPolicy`
(protocol-crate-facing, used by `AgentRegistry::with_trust`) and
`zeph_config::channels::CardTrustPolicy` (TOML-facing, `[a2a_client] card_trust_policy`) — the
same pattern `zeph_mcp::ToolDiscoveryStrategy` uses for its `zeph-config` counterpart, because
`zeph-config` must not depend on protocol crates. Conversion between the two happens at the
`zeph-core` wiring layer once a caller exists (see Current Limitations below).

Configuration: `[a2a_client].card_trust_policy` (default `"ignore"`) and
`[a2a_client].trusted_agent_keys` (list of `{ kid, alg, key_material }` — plain config, not
vault-referenced, since these are public keys). Env override: `ZEPH_A2A_CARD_TRUST_POLICY`.
Setting `card_trust_policy = "require"` while the `card-signing` feature is not compiled in
**fails config load** (`Config::validate`) rather than silently downgrading to `ignore`.

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
- SSRF protection: the address validated by DNS lookup + private-IP check MUST be the exact
  address the client connects to — no re-resolution between validation and connect (see
  Client Security Posture below)
- TLS enforcement: if `require_tls` enabled, `http://` URLs must be rejected, including via redirect
- Server feature (`zeph-a2a?/server`) is independent of client — can run one without the other
- The trust anchor for `AgentCard` signature verification MUST be an out-of-band,
  operator-configured key store (`[a2a_client].trusted_agent_keys`) — NEVER a card-supplied
  `jku` URL. An attacker who can forge an entire card can also point `jku` at a JWKS they
  control and self-sign; auto-fetching `jku` would additionally reopen an SSRF surface this
  crate's transport-layer hardening already guards against (see Client Security Posture below)
- `card_trust_policy = "require"` without the `card-signing` feature compiled in MUST fail
  config load loudly — never silently degrade to `ignore`
- `A2A_PROTOCOL_VERSION` stays at `"0.2.1"` — see A2A 1.0.0 Conformance below before bumping it

---

## Client Security Posture

`A2aClient` accepts a [`SecurityPolicy`] (`crates/zeph-a2a/src/client.rs`) with two independent
flags: `require_tls` and `ssrf_protection`. `SecurityPolicy::hardened()` enables both;
`SecurityPolicy::permissive()` (the `A2aClient::new` default) disables both, for local/dev use
against trusted endpoints.

### SSRF invariant: validated address == connected address

`validate_endpoint` resolves the endpoint hostname via DNS once and returns the resolved
`SocketAddr` list alongside the hostname (`PinnedTarget`). When `ssrf_protection` is on, every
`rpc_call`/`stream_message` request is sent through a per-request `reqwest::Client` built with
`.resolve_to_addrs(host, addrs)`, pinning the connection to those exact addresses. reqwest never
re-resolves the hostname at connect time, so a DNS answer that changes between the check and the
connect (DNS rebinding) cannot redirect the connection to a private/internal address.

**NEVER** discard the resolved addresses after validation and let the underlying HTTP client
re-resolve the hostname independently — that reintroduces the TOCTOU window this invariant closes.

### Redirect policy

Whenever `require_tls` or `ssrf_protection` is enabled, the per-request client is built with
`.redirect(Policy::none())`. A malicious `3xx` response with `Location` pointing at a private
address or downgrading to `http://` is never followed automatically — `rpc_call` (which always
calls `.json()` on the raw response) and `stream_message` (which checks `status.is_success()`)
both surface the unfollowed redirect as an error instead of connecting to `Location`.

### TLS enforcement across hops

`require_tls` rejects any endpoint that does not start with `https://` before the request is
sent, and additionally builds the per-request client with `.https_only(true)`. Combined with the
redirect policy above, a hop cannot downgrade an `https://` connection to `http://` — `https_only`
rejects the plaintext connection outright rather than silently allowing it.

### Secure-by-default config wiring

`A2aClientConfig.require_tls` / `.ssrf_protection` (`crates/zeph-config/src/channels.rs`, nested
under `[a2a_client]`) both default to `true` and are env-overridable
(`ZEPH_A2A_CLIENT_REQUIRE_TLS`, `ZEPH_A2A_CLIENT_SSRF_PROTECTION`). The TUI-remote client
(`src/tui_remote.rs`) builds its `A2aClient` with
`SecurityPolicy { require_tls: config.a2a_client.require_tls, ssrf_protection: config.a2a_client.ssrf_protection }`,
so production deployments are hardened by default through existing config. `[a2a_client]` is
deliberately separate from `[a2a]` (this process's own server config) — see
[`A2aClientConfig`] doc comments for why reusing the server section broke loopback `--connect`
targets (#5878). `A2aServerConfig` never had a matching pair of fields with any effect (the
daemon's own A2A server never read them); the two dead fields it carried for backward
compatibility were removed by #5885, with a `--migrate-config` step dropping any leftover keys
from existing configs.

### Shared resolution helper

The DNS-resolve-then-validate loop lives in `zeph_common::net::resolve_and_validate` (used by both
`zeph-a2a`'s client and `zeph-tools`'s `scrape.rs` web-fetch executor) — do not duplicate this
loop in a new call site; add a caller that maps `zeph_common::net::ResolveError` into the local
error type instead.

---

## IBCT: Invocation-Bound Capability Tokens


IBCT binds each A2A tool invocation to a short-lived capability token carried in the `X-Zeph-IBCT` HTTP header. The token is an HMAC-SHA256 MAC over its own fields — `key_id`, `task_id`, `endpoint`, `issued_at`, `expires_at` — signed with a key from the vault. This scopes the token to a specific task and endpoint and bounds its validity window. It does **not** provide single-use replay protection: there is no `invocation_id`/nonce dedup, so a captured, still-valid token can be replayed against the same `task_id` + `endpoint` until expiry (see Key Invariants).

### Enforcement (#6260)

IBCT is wired end-to-end, opt-in on both sides:

- **Server**: `A2aServer::with_ibct_keys(keys)` (built from `[a2a] ibct_keys`/`ibct_signing_key_vault_ref` in `src/daemon.rs`) installs `zeph_a2a::server::router::ibct_middleware`, layered *inside* `auth_middleware` (bearer auth runs first). When `keys` is non-empty, every `/a2a` and `/a2a/stream` request must carry a valid `X-Zeph-IBCT` header: missing/undecodable → `401`; present but failing `Ibct::verify` (signature, expiry, unknown `key_id`, endpoint/task mismatch) → `403`. An empty key set is a no-op (backward compatible, matches the existing bearer-auth opt-in pattern). The `expected_endpoint` is the server's own advertised `AgentCard::url` (a base URL with no path suffix, shared by both routes).
- **Client**: `A2aClient::with_ibct_key(key)` makes `rpc_call`/`stream_message` call `Ibct::issue` and attach the resulting token as `X-Zeph-IBCT` alongside the bearer token, scoped to the request's `task_id` and to the **origin** of the `endpoint` argument (`ibct_scope_origin` — scheme + host + port, path stripped), not the full per-route URL. This is required, not cosmetic: `/a2a` and `/a2a/stream` are different paths on the same server, but the server verifies both against the one pathless `AgentCard::url`, so a token scoped to a pathful URL can match at most one of the two routes and would 403 on the other (review finding S1 on #6260's implementation PR).
- **`task_id` resolution**: the server reads the expected `task_id` from the request body, dispatching on the JSON-RPC `method` — `params.id` for `tasks/get`/`tasks/cancel`, `params.message.taskId` for everything else (including `/a2a/stream`, which carries no top-level `method`). A brand-new task (no server-assigned ID yet) is checked against the empty-string sentinel; a client issuing its first token for a task calls `Ibct::issue("", endpoint, ttl, key)`. Known limitation: `message/send` always creates a fresh task server-side (never resumes one via `message.taskId`), so every `message/send` request is scoped to the same `""` sentinel rather than a request-specific ID — the invocation-bound property fully holds only for `tasks/get`/`tasks/cancel`.
- **No bundled caller currently issues IBCT tokens.** `src/tui_remote.rs`'s `--connect` usage is the only production `A2aClient` in this repository, and it does not call `with_ibct_key` — it is a remote-TUI-attach client, not a task-delegating one. This PR ships the enforcement *primitive* (server verification + client issuance capability), fully wired and tested, but **enabling `ibct_keys` on a server today does not protect any bundled delegation flow** — there isn't one yet — and will instead `401` `tui_remote` and any standard A2A peer that doesn't know this proprietary header. A real mitigation for the issue's threat model (a compromised delegated subagent replaying a shared bearer token) requires a follow-up change that builds a delegation client calling `with_ibct_key`, wired before an operator turns `ibct_keys` on in production.

### Token Structure

- Algorithm: HMAC-SHA256
- Signed fields: MAC computed over `{key_id}|{task_id}|{endpoint}|{issued_at}|{expires_at}`, hex-encoded into the token's `signature` field
- Wire format: the full token (`key_id`, `task_id`, `endpoint`, `issued_at`, `expires_at`, `signature`) is JSON-serialized, then base64-encoded as a single opaque blob — not a dot-separated triplet
- Header: `X-Zeph-IBCT: <base64-encoded-json>`
- TTL: `ibct_ttl_secs` (default 300 seconds) — tokens older than TTL are rejected by the server, with an additional `CLOCK_SKEW_GRACE_SECS` (30s) grace window on verification

### Key Rotation

`ibct_keys` is a `Vec<IbctKeyConfig>` of `{key_id, key_hex}` entries — `key_hex` is an inline hex-encoded HMAC-SHA256 key (legacy path). `ibct_signing_key_vault_ref` resolves the primary signing key from the vault at startup, constructing an `IbctKey` with `key_id = "primary"`; it takes precedence over `ibct_keys[0]` when both are set. Key rotation is performed by adding a new entry to `ibct_keys` (or rotating the vault-referenced secret) — old tokens signed with retired keys are rejected once their TTL expires.

### Config

```toml
[a2a]
ibct_keys = [
  { key_id = "k1", key_hex = "68656c6c6f2d7365637265742d6b6579" },  # legacy: inline hex key
]
ibct_signing_key_vault_ref = "ZEPH_A2A_IBCT_KEY"  # vault-resolved primary key; takes precedence over ibct_keys[0]
ibct_ttl_secs = 300   # default; token time-to-live in seconds
```

The `ibct` feature flag must be enabled for IBCT to be compiled in.

### Key Invariants

- IBCT is opt-in via the `ibct` feature flag — NEVER enable it by default in builds without the flag
- Token TTL must be enforced at the server side — expired tokens are always rejected, regardless of signature validity
- IBCT tokens are **NOT single-use** — `Ibct::verify` performs no `invocation_id`/nonce dedup. A captured, still-valid token can be replayed against the same `task_id` + `endpoint` until it expires. This is a known limitation, not a documentation gap
- `ibct_signing_key_vault_ref` must resolve to a vault key — startup fails if the ref is set but the vault key is absent
- Key IDs are included in the token header — the verifier must select the correct key by ID, not by position
- NEVER log or dump raw IBCT tokens — they are bearer credentials
- `X-Zeph-IBCT` header must be stripped from any request before forwarding to MCP servers or external tools
- HMAC-SHA256 comparison must use constant-time equality — not `==` (enforced via `Mac::verify_slice`, not `subtle::ConstantTimeEq`)

---

## A2A 1.0.0 Conformance (2026-07-13, #5928)

`A2A_PROTOCOL_VERSION` intentionally stays at `"0.2.1"` even though Agent Card signature
verification (§8.4, one 1.0.0 feature) has been added. Bumping the advertised version would
over-claim full 1.0.0 conformance. Other 1.0.0 deltas remain unimplemented and are explicitly
deferred, not silently dropped:

- Well-known path rename `/.well-known/agent.json` → `/.well-known/agent-card.json`
- gRPC/REST transport bindings (JSON-RPC only today)
- Method-name/field-shape reconciliation against the 1.0.0 spec text

Any change that bumps `A2A_PROTOCOL_VERSION` must first close this gap list (or explicitly
re-scope it) — do not bump the version as a side effect of landing one more 1.0.0 feature.

### Current limitations

- **Live consumer wired to `--connect` only** (#6200, fixed): `AgentRegistry::discover` is now
  called by `src/tui_remote.rs::run_tui_remote` before `zeph --connect <URL>` establishes its
  SSE session — the origin (not the RPC path) is fetched and `card_trust_policy` enforces
  against it, with `require_tls`/`ssrf_protection`/address-pinning applied to the discovery
  fetch itself. A discovery-fetch failure (peer serves no card, network error, timeout) only
  aborts `--connect` under `card_trust_policy = "require"`; under `ignore`/`prefer` it is
  logged and tolerated so a peer serving no agent card at all still connects, matching
  pre-#6200 behavior — a trust-check rejection (untrusted signature or URL-origin mismatch)
  always aborts regardless of policy, since `check_trust` has already folded the policy into
  that verdict (`discovery_error_is_fatal` in `src/tui_remote.rs`). This closes the "no live
  consumer" gap for the `--connect` client path specifically. It remains true that no *agent-
  to-agent delegation* consumer exists — no tool or code path lets the running agent loop
  discover and call an arbitrary peer on its own initiative; `AgentRegistry`'s only live
  caller is the `--connect` attach flow.
- **Unvalidated interop, partially addressed** (#6201): the RFC 8785 JCS canonicalization in
  `card_signing.rs` now strips proto3-default-valued fields (empty string/`false`/`0`/empty
  array/object, recursively) before canonicalizing, closing the specific divergence where a
  real peer that strips defaults before signing but transmits the full card on the wire would
  have its signature incorrectly rejected — covered by a synthetic regression test. This is
  still not validated against a real `a2a-sdk` reference-implementation signed-card vector (no
  network access during development), so `require` remains unproven against real peers in
  general until such a vector is obtained and checked in as a test case; `card_trust_policy`
  intentionally still defaults to `"ignore"`, not `"prefer"`.

---

## Addendum: A2A vs. ANP Positioning (2026-04-17)

Cross-reference: `specs/045-interop-protocol-gaps/spec.md`

### A2A is Zeph's recommended protocol for agent-to-agent delegation

A2A provides centralized discovery via `/.well-known/agent.json` and structured task delegation
via JSON-RPC 2.0. This is sufficient for all current Zeph orchestration use cases:
- DAG subtask delegation to remote specialist agents
- Orchestrator exposing Zeph as a callable agent to external frameworks
- Agent federation via `AgentRegistry` + TTL cache

### ANP is explicitly out of scope (P4 research)

ANP (Agent Network Protocol) offers decentralized, DID-based discovery and capability
re-negotiation designed for open, permissionless agent meshes. Zeph does not implement ANP.

Rationale: centralized A2A discovery covers all current use cases. ANP's DID infrastructure
adds operational complexity with no near-term user benefit. Revisit if Zeph is deployed in
multi-tenant or marketplace environments where arbitrary third-party agent trust is required.

Any proposal to implement ANP must update `specs/045-interop-protocol-gaps/spec.md` and
receive an explicit architectural decision before code is written.
