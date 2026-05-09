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

- [Cocoon project — Telegram](https://cocoon.tg)
- [TON blockchain documentation](https://docs.ton.org)
- [Intel TDX overview](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-trust-domain-extensions.html)
- [OpenAI Chat Completions API reference](https://platform.openai.com/docs/api-reference/chat)

### Internal

| File | Contents |
|---|---|
| `crates/zeph-llm/src/cocoon/mod.rs` | Module root, feature gate |
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
| NFR-4 | Portability | The `cocoon` feature MUST compile cleanly with and without `--features cocoon`; no conditional compilation leakage |
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
├── mod.rs        — module root, feature gate (#[cfg(feature = "cocoon")])
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
| Vault key | age vault | `ZEPH_COCOON_ACCESS_HASH` present (checked only if `cocoon_access_hash` is non-empty in config) |

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

---

## 12. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `CocoonProvider` passes all `LlmProvider` method tests | 100% |
| SC-002 | `zeph cocoon doctor` exits 0 when sidecar is healthy | 100% |
| SC-003 | Feature compiles cleanly with and without `--features cocoon` | 100% |
| SC-004 | Zero new Cargo dependencies introduced | 0 new deps |
| SC-005 | All async I/O paths have tracing spans | 100% coverage |
| SC-006 | clippy `--features cocoon -D warnings` passes | 0 warnings |

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
