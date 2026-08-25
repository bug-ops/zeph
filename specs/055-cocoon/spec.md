---
aliases:
  - Cocoon Integration
  - Cocoon Provider
  - Confidential Compute Open Network
  - CocoonProvider
tags:
  - sdd
  - spec
  - llm
  - providers
  - security
  - contract
  - tee
created: 2026-05-09
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[022-config-simplification/spec]]"
  - "[[038-vault/spec]]"
  - "[[052-gonka-native/spec]]"
---

# Spec: Cocoon Distributed Compute Integration

> [!info]
> Cocoon (Confidential Compute Open Network) is a decentralised AI inference
> platform by Telegram on the TON blockchain. GPU workers run inside Intel TDX
> Trusted Execution Environments (TEEs). From Zeph's perspective Cocoon is a
> **localhost HTTP endpoint** that speaks the OpenAI-compatible wire format.
> All RA-TLS attestation, proxy selection, and TON payments are handled
> transparently by the Cocoon C++ sidecar. Zeph does not interact with the
> TON blockchain, the proxy network, or TEE workers directly.
>
> Epic: [#3681](https://github.com/bug-ops/zeph/issues/3681)

## Sources

### External

- [Cocoon](https://cocoon.org)
- [TON blockchain documentation](https://docs.ton.org)
- [Intel TDX overview](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-trust-domain-extensions.html)
- [OpenAI Chat Completions API reference](https://platform.openai.com/docs/api-reference/chat)

### Internal

| File | Contents |
|---|---|
| `crates/zeph-llm/src/cocoon/mod.rs` | Module root (always compiled — consolidated v0.22.x, spec 029 §3.3) |
| `crates/zeph-llm/src/cocoon/client.rs` | `CocoonClient` — HTTP transport, health check, model listing |
| `crates/zeph-llm/src/cocoon/provider.rs` | `CocoonProvider : LlmProvider` |
| `crates/zeph-llm/src/cocoon/tests.rs` | Unit tests |
| `crates/zeph-llm/src/any.rs` | `AnyProvider` enum — add `Cocoon(CocoonProvider)` variant |
| `crates/zeph-core/src/provider_factory.rs` | `ProviderKind::Cocoon` build path |
| `crates/zeph-config/src/providers.rs` | `ProviderKind::Cocoon` variant + `ProviderEntry` new fields |
| `src/cli/cocoon.rs` | `zeph cocoon doctor` diagnostic subcommand |
| `src/init/llm.rs` | `--init` wizard branch for Cocoon setup |
| `config/default.toml` | Commented-out example `[[llm.providers]]` stanza for Cocoon |

---

## 1. Overview

### Problem Statement

Users of the Telegram ecosystem who have access to the Cocoon distributed
inference network cannot route Zeph inference through it. Cocoon offers
confidential compute via TEE-backed GPU workers; Zeph has no `CocoonProvider`
that speaks its localhost sidecar API.

### Goal

A Zeph user with the Cocoon C++ sidecar running on `localhost:10000` can
declare a `type = "cocoon"` provider in `config.toml`, run `zeph cocoon doctor`
to verify the setup, and route chat, streaming, tool-use, and typed-output
inference through the Cocoon network with full TEE confidentiality guarantees
enforced by the sidecar.

### Out of Scope

The following items are deferred to follow-up issues and are explicitly excluded
from this specification:

> [!danger] Exclusions
> - Sidecar lifecycle management (spawning/supervising the sidecar from Zeph) — issue #3676
> - E2E payload encryption beyond RA-TLS — issue #3677
> - STT via `/v1/audio/transcriptions` — issue #3678
> - Per-token pricing from response headers — issue #3679
> - Native Rust client library replacing the C++ sidecar — issue #3680
> - TON wallet management, private key handling, or staking operations (sidecar owns all TON state)
> - Direct connections to Cocoon proxy or worker nodes (always through sidecar)

---

## 2. User Stories

### US-001: Configure Cocoon as an Inference Provider

AS A Zeph user with the Cocoon sidecar running locally
I WANT to declare `type = "cocoon"` in `[[llm.providers]]` and reference it by name
SO THAT I can route any subsystem's inference through the Cocoon confidential network

**Acceptance criteria:**
```
GIVEN a valid [[llm.providers]] entry with type = "cocoon" and cocoon_client_url set
WHEN Zeph starts
THEN CocoonProvider is constructed, an optional health check is performed against
     /stats, and the provider is registered in AnyProvider
```

### US-002: Diagnose Cocoon Setup

AS A Zeph user setting up Cocoon for the first time
I WANT to run `zeph cocoon doctor`
SO THAT I can confirm the sidecar is reachable, connected to a proxy, and serving
the configured model

**Acceptance criteria:**
```
GIVEN any combination of sidecar availability
WHEN zeph cocoon doctor is executed
THEN a pass/fail table is printed for all six health checks and the process exits
     with code 0 (all pass) or 1 (any fail)
```

### US-003: Interactive Setup via --init Wizard

AS A first-time Zeph user who wants to use Cocoon
I WANT the `--init` wizard to guide me through sidecar URL, optional access hash,
and model selection
SO THAT I end up with a valid config.toml entry without manual editing

**Acceptance criteria:**
```
GIVEN the user selects the Cocoon branch in the --init wizard
WHEN the wizard completes
THEN a [[llm.providers]] entry is written to config.toml and a live model probe
     confirms the sidecar responds with the chosen model
```

### US-004: TUI Cocoon Status and Model Listing

AS A TUI user
I WANT to type `/cocoon status` or `/cocoon models` in the command palette
SO THAT I can inspect sidecar health and available models without leaving the TUI

**Acceptance criteria:**
```
GIVEN Cocoon is configured and the TUI is running
WHEN the user enters /cocoon status
THEN a spinner appears, the /stats endpoint is queried, and the result (proxy_connected,
     worker_count) is displayed in the status area
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-1 | WHEN a `[[llm.providers]]` entry has `type = "cocoon"` THE SYSTEM SHALL implement all `LlmProvider` methods: `chat`, `chat_stream`, `embed`, `chat_with_tools`, `chat_typed` | must |
| FR-2 | WHEN `cocoon_health_check = true` (default) THE SYSTEM SHALL call `GET /stats` at `CocoonProvider` construction time and log a warning if the sidecar is unreachable | must |
| FR-3 | WHEN `CocoonClient::list_models()` is called THE SYSTEM SHALL query `GET /v1/models` and return the list of model ID strings | must |
| FR-4 | WHEN `zeph cocoon doctor` is invoked THE SYSTEM SHALL execute all six health checks (config present, sidecar reachable, proxy connected, workers available, model listed, vault key present if configured) and print a pass/fail table | must |
| FR-5 | WHEN the `--init` wizard is run and the user selects the Cocoon branch THE SYSTEM SHALL prompt for sidecar URL, optional access hash, and model selection, then write a `[[llm.providers]]` stanza to `config.toml` | must |
| FR-6 | WHEN `--migrate-config` is run on an existing config that lacks Cocoon fields THE SYSTEM SHALL apply a no-op migration step that leaves existing configs unchanged | must |
| FR-7 | WHEN the TUI receives a `/cocoon status` command THE SYSTEM SHALL display a spinner, query `/stats`, and render `proxy_connected` and `worker_count` in the status area | must |
| FR-8 | WHEN the TUI receives a `/cocoon models` command THE SYSTEM SHALL display a spinner, query `/v1/models`, and render the model list | must |
| FR-9 | WHEN `cocoon_access_hash` is configured THE SYSTEM SHALL resolve it from the age vault key `ZEPH_COCOON_ACCESS_HASH` at startup and attach it to outgoing requests | must |
| FR-10 | WHEN `cocoon_access_hash` is absent THE SYSTEM SHALL send requests without an access hash header and proceed normally | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-1 | Reliability | All `CocoonClient` HTTP requests MUST use a configurable timeout (default 30 s); no request ever blocks indefinitely |
| NFR-2 | Resilience | WHEN the sidecar is unreachable THE SYSTEM SHALL return `LlmError::Unavailable` without panicking; no `unwrap()` in any Cocoon code path |
| NFR-3 | Observability | All async I/O in the Cocoon module MUST be wrapped in `tracing::info_span!` with names `llm.cocoon.request`, `llm.cocoon.health`, `llm.cocoon.models` |
| NFR-4 | Portability | ~~The `cocoon` feature MUST compile cleanly with and without `--features cocoon`~~ — superseded: the module is always compiled (spec 029 §3.3, 2026-08 audit) |
| NFR-5 | Minimalism | Zero new Cargo dependencies; `reqwest` (already in workspace) is the only HTTP transport needed |
| NFR-6 | Security | `ZEPH_COCOON_ACCESS_HASH` MUST be loaded exclusively from the age vault; never from env vars or plain config fields |
| NFR-7 | Testability | Unit tests MUST cover `CocoonClient` via a local mock server (wiremock pattern); integration tests MUST be gated behind `#[ignore]` |

---

## 5. Architecture

### System Diagram

```
Zeph (AnyProvider::Cocoon)
    │
    ▼
CocoonProvider
    │  delegates body construction + response decoding
    ▼
inner OpenAiProvider (same pattern as GonkaProvider)
    │
    ▼
CocoonClient (HTTP, reqwest, localhost)
    │  RA-TLS handled transparently by sidecar
    ▼
Cocoon C++ sidecar (localhost:10000)
    │  RA-TLS
    ▼
Cocoon Proxy (TEE)
    │  RA-TLS
    ▼
Cocoon Worker (TEE + GPU)
```

### Module Layout

```
crates/zeph-llm/src/cocoon/
├── mod.rs        — module root (always compiled)
├── provider.rs   — CocoonProvider : LlmProvider
├── client.rs     — CocoonClient: HTTP transport, health check, model listing
└── tests.rs      — unit tests (mock server)
```

### Design Rationale

`CocoonProvider` delegates OpenAI-compatible body construction and response
decoding to an inner `OpenAiProvider` (constructed with the sidecar URL).
This reuse avoids duplicating request/response schema logic. The sidecar
speaks standard OpenAI-compatible JSON, so no wire-format changes are needed.
`CocoonClient` provides the transport layer with health checking and model
listing on top of plain `reqwest`.

This pattern mirrors `GonkaProvider`'s delegation to an inner `OpenAiProvider`,
keeping the codebase DRY.

---

## 6. Config Schema

### Example TOML

```toml
[[llm.providers]]
name                = "cocoon"
type                = "cocoon"
model               = "Qwen/Qwen3-0.6B"
cocoon_client_url   = "http://localhost:10000"
cocoon_access_hash  = ""       # leave empty; resolved from vault as ZEPH_COCOON_ACCESS_HASH
cocoon_health_check = true
max_tokens          = 4096
```

### Rust Types (`crates/zeph-config/src/providers.rs`)

```rust
// ProviderKind variant (added alongside Gonka, Compatible, etc.)
Cocoon,

// New fields added to ProviderEntry:
pub cocoon_client_url:   Option<String>,   // default "http://localhost:10000"
pub cocoon_access_hash:  Option<String>,   // resolved from vault; plain field left empty
pub cocoon_health_check: bool,             // default true
```

### Vault Key

| Key | Usage |
|-----|-------|
| `ZEPH_COCOON_ACCESS_HASH` | Optional access hash for authenticated Cocoon networks; resolved at startup; never stored in plain config |

> [!warning]
> The `cocoon_access_hash` field in `config.toml` MUST be left empty.
> The actual value is always resolved from the age vault as `ZEPH_COCOON_ACCESS_HASH`.
> Sidecar TON wallet management is fully opaque to Zeph.

---

## 7. Core Abstractions

### `CocoonClient`

```rust
// crates/zeph-llm/src/cocoon/client.rs
pub struct CocoonClient {
    base_url:     String,
    access_hash:  Option<String>,
    client:       reqwest::Client,
    timeout:      Duration,
}

impl CocoonClient {
    pub async fn health_check(&self) -> Result<CocoonHealth, LlmError>;
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError>;
    pub async fn post(&self, path: &str, body: &[u8]) -> Result<reqwest::Response, LlmError>;
}

pub struct CocoonHealth {
    pub proxy_connected: bool,
    pub worker_count:    u32,
}
```

### `CocoonProvider`

```rust
// crates/zeph-llm/src/cocoon/provider.rs
pub struct CocoonProvider {
    inner:     OpenAiProvider,       // body construction + response decode
    client:    Arc<CocoonClient>,
    usage:     UsageTracker,
    pub(crate) status_tx: Option<StatusTx>,
}

impl LlmProvider for CocoonProvider {
    // All methods delegate to inner.build_request_body()
    // then client.post() for transport
    // then inner.decode_response()
}
```

### `LlmProvider` Method Table

| Method | Behaviour |
|--------|-----------|
| `chat` | Build body via inner `OpenAiProvider`; send via `client.post`; decode |
| `chat_stream` | As `chat` but request SSE stream from sidecar |
| `chat_with_tools` | Tools-enabled body (OpenAI tools format); send; decode `tool_calls` array |
| `chat_typed` | Typed structured output (`json_schema` response format); send; decode |
| `embed` | Delegate to inner `OpenAiProvider`; sidecar exposes `/v1/embeddings` |
| `supports_streaming` | `true` |
| `supports_embeddings` | `true` (if sidecar model supports it; runtime check) |
| `supports_tool_use` | `true` |
| `supports_vision` | `false` (deferred) |
| `name` | Provider name from config (e.g., `"cocoon"`) |
| `last_usage` | Parsed from `usage` field in OpenAI-format response |

---

## 8. Doctor Command Health Checks

`zeph cocoon doctor [--json] [--timeout-secs N]`

| Check | Endpoint | Pass Condition |
|-------|----------|----------------|
| Config present | `config.toml` | `type = "cocoon"` entry exists |
| Sidecar reachable | `GET /stats` | HTTP 200 in < 5 s |
| Proxy connected | `/stats` JSON | `proxy_connected: true` |
| Workers available | `/stats` JSON | `worker_count > 0` |
| Model listed | `GET /v1/models` | Configured model ID appears in response |
| Vault key | age vault | `ZEPH_COCOON_ACCESS_HASH` present (checked only if `cocoon_access_hash` is **present** in config, i.e. `Some(_)`) |

Exit code: 0 if all applicable checks pass, 1 otherwise.

```
zeph cocoon doctor
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Config present                    ✓
Sidecar reachable                 ✓ 12 ms
Proxy connected                   ✓ true
Workers available                 ✓ 3 workers
Model listed (Qwen/Qwen3-0.6B)    ✓
Vault key ZEPH_COCOON_ACCESS_HASH  - not configured (skipped)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Result: 5/5 checks passed
```

---

## 9. Integration Points

| Subsystem | Change |
|-----------|--------|
| `zeph-llm/src/any.rs` | Add `Cocoon(CocoonProvider)` variant to `AnyProvider` |
| `zeph-core/src/provider_factory.rs` | Add `ProviderKind::Cocoon => build_cocoon_provider(entry, config)` arm |
| Agent loop | No changes — dispatched transparently via `AnyProvider` |
| Orchestrator | No changes — referenced by name in multi-model configs |
| TUI | `/cocoon status` and `/cocoon models` palette entries; spinner during inference; TON balance displayed in sidebar (from `/stats` response) |
| CLI | `zeph cocoon doctor [--json] [--timeout-secs N]` subcommand |
| `--init` wizard | New branch: sidecar URL prompt → access hash prompt (optional) → model probe → model selection → config write |
| `--migrate-config` | No-op migration step — new optional fields have defaults, no existing config breaks |

---

## 10. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Sidecar not running at startup (`cocoon_health_check = true`) | Log `WARN` with actionable message; provider construction succeeds; inference attempts return `LlmError::Unavailable` |
| Sidecar not running at inference time | `CocoonClient::post` returns `LlmError::Unavailable`; no panic |
| `proxy_connected: false` in `/stats` | Doctor reports check failure; provider still constructed (proxy may reconnect) |
| `worker_count: 0` in `/stats` | Doctor reports check failure; inference may still be queued by sidecar |
| Sidecar returns HTTP 5xx | `LlmError::ServerError` with status code; no retry (sidecar handles retries internally) |
| `ZEPH_COCOON_ACCESS_HASH` missing when access hash is configured | `LlmError::AuthenticationFailed` at startup; actionable vault error message |
| Request timeout (> 30 s) | `tokio::time::timeout` fires; `LlmError::Timeout` returned |
| Model not in `/v1/models` response | Doctor reports check failure; inference proceeds anyway (sidecar may still serve it) |
| Malformed JSON from sidecar | `LlmError::ParseError` with raw bytes logged at `TRACE` level |
| Feature compiled without `cocoon` flag | `ProviderKind::Cocoon` arm is unreachable; startup emits `LlmError::Unsupported` at provider construction |

---

## 11. Key Invariants

### Always

- Every HTTP call from `CocoonClient` is wrapped in `tokio::time::timeout(self.timeout, …)`
- `ZEPH_COCOON_ACCESS_HASH` is loaded exclusively from the age vault — never from env vars or plain config values
- Tracing spans are present on all async I/O: `llm.cocoon.request`, `llm.cocoon.health`, `llm.cocoon.models`
- All requests go through the local sidecar; no direct connections to Cocoon proxy or workers
- `LlmError::Unavailable` is returned (never a panic) when the sidecar is unreachable

### Ask First

- Changing `CocoonHealth` response fields (depends on sidecar `/stats` schema)
- Adding new HTTP headers beyond access hash (may require sidecar protocol update)
- Enabling STT via `/v1/audio/transcriptions` (deferred to issue #3678)

### Never

> [!danger] Hard Constraints
> - NEVER embed TON private keys in Zeph config — sidecar manages its own wallet
> - NEVER connect directly to Cocoon proxy or worker nodes — always through sidecar
> - NEVER bypass RA-TLS — sidecar enforces this transparently; Zeph must not implement its own RA-TLS
> - NEVER hardcode port numbers — always read from `cocoon_client_url` config field
> - NEVER implement TON crypto operations in Zeph — delegate entirely to sidecar
> - NEVER store Cocoon payment state in Zeph's SQLite — sidecar owns all payment and balance state
> - NEVER use `openssl-sys` — rustls everywhere per constitution
> - NEVER assume Qdrant data is TEE-protected when using Cocoon — Qdrant runs outside the TEE; only inference computation on the worker is protected
> - NEVER implement compound attestation verification without upstream Cocoon sidecar support for forwarding attestation evidence — Zeph cannot verify the chain end-to-end unilaterally
> - NEVER enable `cocoon_managed = true` by default — must be explicit opt-in because managing the sidecar lifecycle places Zeph inside the trusted compute base and weakens TEE attestation guarantees

---

## 12. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `CocoonProvider` passes all `LlmProvider` method tests | 100% |
| SC-002 | `zeph cocoon doctor` exits 0 when sidecar is healthy | 100% |
| SC-003 | ~~Feature compiles cleanly with and without `--features cocoon`~~ — superseded, always compiled | N/A |
| SC-004 | Zero new Cargo dependencies introduced | 0 new deps |
| SC-005 | All async I/O paths have tracing spans | 100% coverage |
| SC-006 | clippy `-D warnings` passes | 0 warnings |

---

## 13. Open Questions

> [!question]
> - The `/stats` JSON schema for the Cocoon sidecar is not yet publicly documented.
>   Implementation should treat extra fields as unknown and parse defensively.
> - Whether `embed` should return `LlmError::Unsupported` (like GonkaProvider) or
>   delegate to the sidecar depends on which models are served. Initial implementation
>   should attempt delegation and fall back to `Unsupported` if the sidecar returns 404.

---

## 14. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[001-system-invariants/spec]] — Cross-cutting invariants
- [[003-llm-providers/spec]] — `LlmProvider` trait and `AnyProvider` enum
- [[022-config-simplification/spec]] — `[[llm.providers]]` canonical format and `ProviderEntry`
- [[038-vault/spec]] — Age vault backend, zeroize-on-drop guarantee
- [[052-gonka-native/spec]] — Analogous native transport pattern (GonkaProvider)
- [[055-cocoon/threat-model]] — TEE threat model: six-layer stack mapping and nine security goals (arXiv:2605.03213)

---

## 15. Security and Trust Model

> [!info]
> This section documents the TEE trust boundary for the Zeph/Cocoon integration,
> known security limitations, and operator guidance. It is based on threat model
> analysis from arXiv:2605.03213. See [[055-cocoon/threat-model]] for the full
> mapping of the six-layer agent stack and nine security goals.

### 15.1 TEE Trust Boundary

The Cocoon integration provides TEE-backed confidential inference for the
**computation only**. The trust boundary is narrow and must be understood by
operators choosing Cocoon for privacy-sensitive workloads.

```
Zeph process (no TEE)
    │  plaintext prompt assembled here
    ▼
Context assembly + Qdrant memory (no TEE)
    │  plaintext prompt crosses trust boundary
    ▼
CocoonClient → localhost HTTP → Cocoon C++ sidecar (no TEE)
    │  RA-TLS — channel is encrypted
    ▼
Cocoon Proxy (TEE — Intel TDX)
    │  RA-TLS — channel is encrypted; worker is attested
    ▼
Cocoon Worker (TEE + GPU — Intel TDX + NVIDIA H100 CC)
    └── inference computation protected inside TEE
```

**What is TEE-protected:** inference computation on the worker node. Prompt
and response content are processed inside the TEE worker; the worker's memory
is inaccessible to the host OS or cloud provider.

**What is NOT TEE-protected:** everything upstream of the sidecar, including
Zeph process memory, SQLite conversation history, Qdrant embeddings and
retrieved context, and the localhost plaintext segment between Zeph and the
sidecar.

### 15.2 Known Limitations

> [!danger] Known Limitations — read before choosing Cocoon for confidential workloads
>
> **1. Compound attestation gap**
> Zeph trusts the sidecar implicitly via localhost without verifying the
> attestation chain end-to-end. The sidecar verifies proxy attestation via
> RA-TLS and the proxy verifies worker attestation. However, Zeph has no way
> to verify that the sidecar's RA-TLS connection leads to a genuine TEE worker
> rather than a proxy that terminated the RA-TLS session. True compound
> attestation would require the sidecar to forward attestation evidence to
> Zeph — this is a protocol-level capability gap in the current Cocoon design,
> not something Zeph can resolve unilaterally.
>
> **2. Qdrant memory outside the TEE**
> Conversation history, extracted knowledge graphs, and semantic embeddings are
> stored in SQLite and Qdrant, both running entirely outside any TEE. When Zeph
> assembles context for a Cocoon inference request, the retrieved content crosses
> the TEE boundary in plaintext. The TEE only protects the computation on the
> worker; it does not protect data at rest in Qdrant or in transit through the
> context assembly pipeline. Full protection would require a TEE-backed vector
> database — not feasible with the current architecture.
>
> **3. Localhost-only validation and containerised deployments**
> `cocoon_client_url` is validated to accept only localhost addresses
> (`localhost`, `127.0.0.1`, `::1`). This is correct for bare-metal: the sidecar
> MUST be co-located to preserve confidentiality (non-localhost HTTP exposes
> plaintext prompts on the network). In Kubernetes pods, containers share a
> network namespace, so `localhost` works. In Docker Compose with separate
> container services (e.g., `cocoon-sidecar:10000`), the sidecar address is a
> container hostname rather than `localhost`. Docker Compose users who run the
> sidecar as a separate service MUST use host networking (`network_mode: host`)
> or a shared network namespace to satisfy the localhost constraint. Relaxing
> the localhost restriction to allow arbitrary remote hosts would negate TEE
> confidentiality benefits and is not planned.
>
> **4. `ton_balance` side-channel (MITIGATED #4649, #4657)**
> `CocoonHealth.ton_balance` is returned by `/stats` and displayed in the TUI
> sidebar. In shared-access or shared-screen scenarios, an observer with TUI
> visibility can infer the user's spending volume and usage pattern from
> balance changes over time. This is mitigated by `cocoon.show_balance` (default `true`):
> when `false`, the TUI renders `*** TON` instead of the real balance value.
> Operators in multi-user or shared-screen environments should set `show_balance = false`.
>
> ```toml
> [cocoon]
> show_balance = true   # set false to redact balance in TUI sidebar
> ```
>
> Migration step 53 (config 2026-05 series) adds a commented `[cocoon]` section to
> existing configs for discoverability. The `--init` wizard prompts for this value
> in the Cocoon setup section.
>
> **5. GPU-TEE overhead**
> Intel TDX provides CPU-level TEE protection; NVIDIA H100 Confidential
> Computing provides GPU-level TEE protection. Running inference inside a
> GPU-TEE incurs 10–30% latency overhead compared to non-confidential GPU
> inference (per arXiv:2605.03213). This is a Cocoon infrastructure
> characteristic, not a Zeph implementation issue, but operators making
> provider selection decisions should account for it.

### 15.3 TEE Attestation Chain

The attestation chain Zeph relies on is:

1. **Sidecar ↔ Proxy**: RA-TLS — the sidecar verifies the proxy's TDX
   attestation quote before establishing a connection. If attestation fails, the
   sidecar refuses to connect (transparent to Zeph).
2. **Proxy ↔ Worker**: RA-TLS — the proxy verifies the worker's TDX+GPU
   attestation quote. Zeph has no visibility into this sub-chain.
3. **Zeph ↔ Sidecar**: plain localhost HTTP. No attestation. Zeph trusts the
   sidecar by virtue of it running on the same host.

**Gap**: Zeph cannot verify the full chain end-to-end. The `proxy_connected`
and `worker_count` fields from `/stats` are informational signals, not
cryptographic attestation evidence. A future `cocoon attestation` command
could fetch chain evidence if the sidecar exposes it, but this requires
upstream Cocoon protocol support.

### 15.4 Updated Key Invariants (Security)

Add the following to Section 11 "Key Invariants":

**Never (additional):**

> [!danger]
> - NEVER assume Qdrant data is TEE-protected when using Cocoon — Qdrant runs
>   outside the TEE; only inference computation is protected
> - NEVER implement compound attestation verification without upstream Cocoon
>   sidecar support for forwarding attestation evidence
> - NEVER enable `cocoon_managed = true` by default — explicit opt-in required
>   because managing the sidecar lifecycle makes Zeph part of the trusted
>   compute base and weakens TEE attestation guarantees

### 15.5 Compound Attestation Monitoring Checklist

**Tracking issue:** #4650 (P2, upstream-blocked). This section defines the
monitoring criteria that determine when #4650 becomes implementable. The issue
remains open as the canonical tracking anchor — do not close it until all
criteria below are resolved and an implementation PR is merged.

**Cross-ref:** threat `T-COMP-ATTEST` in [[055-cocoon/threat-model]] §4;
SG-7 in §2; Challenge 1 in §3. Also tracked in the CI dependency-watch loop
(`.claude/rules/continuous-improvement.md`, "Dependency Monitoring" section).

#### What to watch in Cocoon releases

Monitor Cocoon sidecar release notes and API documentation for any of the
following signals:

| Signal | What it means |
|--------|---------------|
| New endpoint `GET /attestation` or `GET /attestation/evidence` | Sidecar exposes TDX quote and/or proxy certificate chain directly |
| `GET /health` or `GET /stats` response gains a `tdx_quote` or `attestation` field | Attestation evidence embedded in existing health endpoint |
| `GET /stats` response gains a `proxy_cert_chain` or `pcr_values` field | Proxy certificate chain or platform configuration registers exposed |
| Release note mentions "attestation evidence", "TDX quote", "compound attestation", "PCK cert", "remote attestation API", or "quote forwarding" | Protocol-level capability landing |
| New sidecar capability negotiation field (version handshake, capability flags) | May indicate E2E or attestation feature availability |

#### Trigger → action

When **any** of the above signals appear in a Cocoon release:

1. Comment on issue #4650 with the Cocoon version, the signal observed, and
   a link to the release notes or API diff.
2. File a new P2 implementation issue: `feat(cocoon): implement compound
   attestation verification (#4650 follow-up)`. Body must include:
   - The attestation evidence endpoint and response schema
   - Implementation plan: fetch TDX quote + PCK cert chain; validate quote
     signature against Intel root CA; surface pass/fail in `cocoon doctor`
     output; add config flag `cocoon.verify_attestation_chain` (default `false`
     until validation is battle-tested)
   - Integration: add `cocoon attestation verify` CLI subcommand; TUI status
     indicator; tracing span `llm.cocoon.attestation`
3. Re-read §11 "Never" invariants before implementing — the constraint
   "NEVER implement compound attestation verification without upstream Cocoon
   sidecar support" becomes satisfiable once the endpoint exists.

#### What NOT to do

- Do not implement attestation verification by screen-scraping `cocoon doctor`
  CLI output — require a machine-readable API endpoint.
- Do not assert `proxy_connected = true` is equivalent to attestation — it is
  an informational signal from the sidecar itself, not an independently
  verifiable cryptographic proof (see §15.3).
- Do not embed this checklist as the recurring trigger mechanism — the CI
  dependency-watch loop in `.claude/rules/continuous-improvement.md` watches
  sidecar dependency updates; cross-link #4650 there so automated cycles flag it.

---

## 16. Deferred Features — Research Findings

This section records research outcomes for features explicitly deferred from
the initial Cocoon implementation. Each entry documents the design decision,
the config interface reserved for future use, and acceptance criteria for
eventual implementation.

### 16.1 Sidecar Lifecycle Management (Issue #3676)

**Status:** DEFERRED to post-v1.0.0 (P3)

**Research question answered:** Should Zeph spawn and supervise the Cocoon
C++ sidecar process?

**Recommendation:** No. Spawning the sidecar from Zeph makes Zeph part of the
trusted compute base. The sidecar performs RA-TLS attestation proving to the
proxy that it is running in a TEE-safe environment. If Zeph controls the
sidecar's lifecycle, an adversary who compromises Zeph could substitute a
modified sidecar binary. This weakens the TEE guarantees that Cocoon provides.
The current model — user starts the sidecar independently, Zeph connects to it
— maintains a clean trust separation.

**Additional deferral reasons:**
- Managing a C++ binary from Rust requires platform-specific process
  semantics, binary location discovery, argument handling, and version pinning
  that add complexity without clear UX benefit (`cocoon doctor` already gives
  actionable guidance when the sidecar is down).
- Distribution questions: the Cocoon sidecar has its own release cycle;
  bundling it with Zeph raises packaging, platform, and update concerns.

**Config interface reserved (not implemented):**

```toml
# cocoon_managed = false           # default; set true to let Zeph own sidecar lifecycle
# cocoon_binary_path = ""          # path to cocoon-client binary; required if cocoon_managed = true
# cocoon_args = []                 # extra args passed to sidecar at spawn
```

> [!note]
> The config stanzas above are intentionally kept commented-out. Promoting them
> to live schema without the supervisor-backed implementation would create a
> half-wired feature flag that violates the MVP "no half-finished implementations"
> rule (CLAUDE.md). These keys remain reserved; do not add deserialization or
> validation until the full #3676 implementation lands.

**Future implementation acceptance criteria (post-v1.0.0):**
- GIVEN `cocoon_managed = true` and `cocoon_binary_path` set
- WHEN Zeph starts and `/stats` is unreachable
- THEN Zeph spawns the binary via `TaskSupervisor::spawn_restartable` (per
  `zeph_common::task_supervisor`, spec-039) with a `RestartPolicy` providing
  exponential backoff and a circuit breaker (max 3 retries before giving up);
  registers a SIGTERM hook for clean shutdown
- Designated managed entry point: `cocoon doctor --start` (explicit operator
  action; avoids implicit spawn at startup which would weaken the trust-boundary
  argument; operator-initiated makes the trust decision explicit)
- Platform: Unix-only initially; Windows deferred
- MUST document in operator guide that `cocoon_managed = true` weakens TEE
  attestation guarantees
- MUST NOT use a raw `tokio::spawn` — all lifecycle management MUST go through
  `TaskSupervisor::spawn_restartable` (spec-039 "NEVER" constraint)

> [!note] Cross-reference with #4650
> If compound attestation (#4650) lands before this feature is implemented, the
> trust-boundary objection to `cocoon_managed` weakens: Zeph could cryptographically
> verify the sidecar's attestation chain even when it spawned the sidecar.
> Re-evaluate the deferral at that point.

### 16.2 End-to-End Payload Encryption (Issue #3677)

**Status:** DEFERRED (P3)

**Research question answered:** Is E2E payload encryption beyond RA-TLS
feasible and warranted?

**Recommendation:** The feature is technically feasible and the Cocoon protocol
supports it per Cocoon documentation (Ed25519 keypair, public key in request,
worker encrypts response, client decrypts). Deferral is due to implementation
complexity — key management, streaming encryption, vault integration — not
because the feature is blocked on upstream. The open question is whether
encryption should happen at the sidecar level or at the client (Zeph) level.

**Threat model context:** RA-TLS already encrypts the sidecar ↔ proxy ↔
worker channel. E2E encryption provides additional defense-in-depth against a
compromised proxy node that terminates RA-TLS but does not have access to
worker TEE memory. This is a narrow but real threat in a decentralised
multi-operator proxy network.

**Protocol sketch (from Cocoon docs):**
1. Zeph generates an Ed25519 keypair at startup
2. Private key stored in age vault as `ZEPH_COCOON_E2E_PRIVATE_KEY`
3. Public key sent in request header `X-Cocoon-E2E-Pubkey`
4. Worker encrypts response payload with the public key
5. Zeph decrypts with the private key

**Key design question:** The issue's primary open question — whether
encryption is performed by the Cocoon sidecar on behalf of the client, or
by Zeph directly before sending to the sidecar — must be resolved with the
Cocoon upstream before implementation. Option A (Zeph encrypts, sidecar passes
opaque ciphertext) is the stronger model and the recommended path.

Option B (sidecar encrypts after receiving plaintext from Zeph) provides no
security improvement over RA-TLS alone **when Zeph and the sidecar share a
loopback interface (the supported default for bare-metal deployments)**. However,
this reasoning does not hold universally: in containerised deployments where
Zeph and the sidecar run in separate containers on a real container network
(see §15.2 Limitation #3 — Docker Compose with separate services, or Kubernetes
with separate pods), the Zeph→sidecar hop is NOT a trusted loopback and Option B
WOULD provide additional protection for that segment. Option A remains preferred
because it protects all topologies; Option B is not ruled out for containerised
threat models where the topology diverges from the bare-metal default.

**Open question:** Is client-side E2E (Option A) supported by the current
Cocoon sidecar API, or is it experimental/unimplemented? This must be confirmed
with upstream before implementation begins.

**Performance note:** The latency overhead of asymmetric Ed25519/X25519 per
request for typical prompt sizes is expected to be negligible; SSE per-chunk
AEAD streaming is more complex (see deferred challenges below). These claims are
**not measured** — no live sidecar or benchmarks are available. Performance
impact MUST be benchmarked at implementation time; do not treat qualitative
assessments here as measured results.

**Config interface reserved (not implemented):**

```toml
# cocoon_e2e_encryption = false    # default; enable Ed25519 E2E encryption if supported by sidecar
```

> [!note]
> `cocoon_e2e_encryption` is intentionally kept commented-out until the upstream
> Option-A question is resolved and a full implementation is ready. Promoting it
> to live schema now would create a half-wired flag with no consumer, violating
> the MVP rule. The vault key slot `ZEPH_COCOON_E2E_PRIVATE_KEY` is also reserved
> but not wired; do not add vault lookup until #3677 implementation begins.

**Vault key reserved:**

| Key | Usage |
|-----|-------|
| `ZEPH_COCOON_E2E_PRIVATE_KEY` | Ed25519 private key for E2E encryption; generated at setup |

**Deferred challenges:**
- Key rotation mid-conversation requires protocol support
- SSE streaming: per-chunk encryption requires a streaming cipher mode (AEAD
  per chunk), significantly more complex than request/response encryption
- Older sidecar versions without E2E support require graceful fallback

**Future implementation acceptance criteria (post-v1.0.0):**
- GIVEN `cocoon_e2e_encryption = true` and `ZEPH_COCOON_E2E_PRIVATE_KEY` in vault
- WHEN Zeph sends a chat or embed request
- THEN the Ed25519 public key is included in the request header and the
  response is decrypted before deserialisation
- MUST confirm with Cocoon upstream that Option A (client-side encryption) is
  supported and at GA maturity before starting implementation (see §17 open
  questions: "E2E encryption point" and "E2E encryption maturity")
- MUST benchmark Ed25519/X25519 overhead and SSE streaming AEAD per-chunk cost
  at implementation time; no performance estimates are asserted in this spec
- MUST address containerised topology interaction (§17 "Containerised topology
  interaction with E2E") if the deployment topology differs from the
  bare-metal default

---

## 17. Updated Open Questions

> [!question]
> The following questions extend the original open questions in Section 13:
>
> - **`/stats` schema stability**: `CocoonHealth` parses `proxy_connected`,
>   `worker_count`, `ton_balance` with `#[serde(default)]`. If Cocoon changes
>   the schema, parsing silently succeeds with defaults. Is there a versioning
>   mechanism or a way to detect schema drift?
> - **Attestation evidence endpoint**: Does the Cocoon sidecar expose
>   attestation chain information (proxy certificate details, TDX quote)? This
>   would enable partial compound attestation verification from Zeph. See §15.5
>   for the monitoring checklist; tracking issue #4650.
> - **E2E encryption point (Option A vs. B)**: Does the current Cocoon sidecar
>   support E2E encryption at the client level (Zeph encrypts before sending —
>   Option A) or only at the sidecar level (Option B)? This determines which
>   option is viable for #3677. Option A is the security-positive path; Option B
>   provides no protection in the default bare-metal topology (see §16.2).
> - **E2E encryption maturity (GA vs. experimental)**: Even if Option A is
>   supported by the sidecar protocol, is this feature generally available or
>   experimental/unstable in the current Cocoon release? Must be confirmed with
>   upstream before starting #3677 implementation.
> - **Containerised topology interaction with E2E**: In Docker Compose or
>   Kubernetes deployments where Zeph and the sidecar are in separate network
>   namespaces (§15.2 Limitation #3), does the Cocoon sidecar support Option A
>   E2E encryption for the inter-container hop? Does the deployment guide
>   address this topology?
> - **Streaming E2E**: If E2E encryption is added, what cipher mode does Cocoon
>   use for SSE streaming chunks? Per-chunk AEAD, or a session cipher with
>   stream continuity?
> - **Cost model for TEE**: `cocoon_pricing` allows manual per-1K-token pricing.
>   Does the sidecar expose real-time cost estimates that Zeph could use for
>   dynamic cost tracking?
