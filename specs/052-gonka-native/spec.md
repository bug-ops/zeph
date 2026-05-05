---
aliases:
  - Gonka Native Transport
  - GonkaProvider
  - Gonka Phase 2
tags:
  - sdd
  - spec
  - llm
  - providers
  - security
  - contract
created: 2026-05-05
status: draft
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[022-config-simplification/spec]]"
  - "[[038-vault/spec]]"
  - "[[051-gonka-gateway/spec]]"
---

# Spec: Gonka.ai Native Transport (Phase 2)

> [!info]
> Phase 2 integration of gonka.ai inference using direct node communication.
> Introduces `GonkaProvider` in `crates/zeph-llm/src/gonka/`: ECDSA secp256k1
> request signing, bech32 address derivation, an `EndpointPool` with round-robin
> fail-skip, and a `send_signed_with_retry` contract. Body and decode logic
> are delegated to an inner `OpenAiProvider`; only the signing and transport
> layer is new.

## Sources

### External
- [Gonka.ai network overview](https://gonka.ai)
- [k256 crate (secp256k1 ECDSA)](https://docs.rs/k256)
- [bech32 crate](https://docs.rs/bech32)
- [ripemd crate](https://docs.rs/ripemd)
- [sha2 crate](https://docs.rs/sha2)
- [inferenced CLI](https://github.com/gonka-ai/inferenced) — external key management tool

### Internal
| File | Contents |
|---|---|
| `crates/zeph-llm/src/gonka/mod.rs` | Module root: re-exports, `GonkaProvider` struct |
| `crates/zeph-llm/src/gonka/signer.rs` | `RequestSigner` — lock-free ECDSA signing, address derivation |
| `crates/zeph-llm/src/gonka/pool.rs` | `EndpointPool` — round-robin endpoint selection, fail-skip |
| `crates/zeph-llm/src/gonka/transport.rs` | `send_signed_with_retry` — timeout, re-sign per retry, HTTP dispatch |
| `crates/zeph-llm/src/gonka/provider.rs` | `LlmProvider` impl for `GonkaProvider` |
| `crates/zeph-llm/src/any.rs` | `AnyProvider` enum — add `Gonka(GonkaProvider)` variant |
| `crates/zeph-llm/src/lib.rs` | Feature-gate or unconditional re-export of `gonka` module |
| `crates/zeph-config/src/llm.rs` | `ProviderKind::Gonka`, `GonkaNode`, `gonka_nodes`, `gonka_chain_prefix` config fields |
| `src/init/llm.rs` | Wizard branch for native gonka setup |
| `src/cli/gonka.rs` | `zeph gonka doctor` diagnostic subcommand |
| `config/default.toml` | Commented-out `[[llm.providers]]` stanza for native gonka |

---

## 1. Overview

### Problem Statement

The [[051-gonka-gateway/spec|GonkaGate gateway]] (Phase 1) relies on a hosted
intermediary. Users who want permissionless access — connecting directly to
gonka network nodes — need a transport that can sign requests with their gonka
private key using the network's ECDSA secp256k1 scheme. No such transport exists
in Zeph today.

### Goal

A Zeph user with a gonka private key (exported via the `inferenced` CLI) can
configure a `gonka` provider, run `zeph gonka doctor` to verify the setup, and
route inference directly through the gonka network with automatic endpoint
fail-over and per-request signing.

### Out of Scope

- Native Rust Cosmos wallet (BIP39/BIP32 key derivation) — key management is delegated to the external `inferenced` CLI
- GNK token purchase, staking, or faucet integration
- Multi-account support per process (one private key per running Zeph instance)
- Vision input (gonka catalog is text-only at launch)
- End-to-end staking flow

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a `[[llm.providers]]` entry has `type = "gonka"` THE SYSTEM SHALL resolve `ZEPH_GONKA_PRIVATE_KEY` and `ZEPH_GONKA_ADDRESS` from the age vault at startup | must |
| FR-002 | WHEN sending any request to a gonka node THE SYSTEM SHALL sign the request body with ECDSA secp256k1 and set `Authorization`, `X-Requester-Address`, and `X-Timestamp` headers | must |
| FR-003 | WHEN a gonka node returns HTTP 5xx or a network error THE SYSTEM SHALL skip that node and retry on the next node in the `EndpointPool` with a freshly computed timestamp | must |
| FR-004 | WHEN all endpoints in the pool are exhausted THE SYSTEM SHALL return `LlmError::AllEndpointsFailed` | must |
| FR-005 | WHEN the `--init` wizard is run and the user selects "Gonka.ai (native)" THE SYSTEM SHALL guide the user through `inferenced` key export, vault storage, and a self-test probe | must |
| FR-006 | WHEN `zeph gonka doctor` is invoked THE SYSTEM SHALL verify vault key presence, address derivation, endpoint reachability, and signing round-trip, printing a pass/fail table | must |
| FR-007 | WHEN `gonka_chain_prefix` is set in config THE SYSTEM SHALL use that prefix for bech32 address encoding; default is `"gonka"` | must |
| FR-008 | WHEN a gonka node returns HTTP 401 THE SYSTEM SHALL classify the failure as a non-retryable `LlmError::AuthenticationFailed` and not attempt other nodes | must |
| FR-009 | WHEN a gonka request exceeds the configured `request_timeout` THE SYSTEM SHALL cancel the request and mark that endpoint as temporarily failed | must |
| FR-010 | WHEN `GonkaProvider::chat_stream` is called THE SYSTEM SHALL stream SSE chunks via the same signed transport, falling back to non-streaming if the node does not support it | should |

---

## 3. Architecture

### Module Layout

```
crates/zeph-llm/src/gonka/
├── mod.rs          — re-exports, GonkaProvider struct definition
├── signer.rs       — RequestSigner (lock-free)
├── pool.rs         — EndpointPool (round-robin + fail-skip)
├── transport.rs    — send_signed_with_retry
└── provider.rs     — LlmProvider impl
```

### Transport Design Rationale

`GonkaProvider` delegates body serialisation and response decoding to an inner
`OpenAiProvider` (constructed with a dummy URL that is never used for dispatch).
This reuse avoids duplicating the OpenAI-compatible request/response schema.
The signed transport replaces only the HTTP send step:

```
GonkaProvider::chat(messages)
    │
    ├── inner.build_request_body(messages) → Vec<u8>   (OpenAI format)
    │
    ├── signer.sign(&body, timestamp_ns) → headers
    │
    ├── pool.next_endpoint() → url
    │
    ├── send_signed_with_retry(url, headers, body)
    │       └── on error/5xx → pool.mark_failed(url); re-sign; retry
    │
    └── inner.decode_response(bytes) → String
```

> [!warning]
> The body bytes passed to `sign()` and to `reqwest::Body::from(Vec<u8>)` must
> be byte-identical. Never re-serialise between signing and sending.

---

## 4. RequestSigner

### API

```rust
pub struct RequestSigner {
    signing_key: k256::ecdsa::SigningKey,   // derived from private key bytes
    address:     String,                    // bech32 address, precomputed at init
}

impl RequestSigner {
    pub fn new(private_key_bytes: &[u8], chain_prefix: &str) -> Result<Self, SignerError>;
    pub fn sign(&self, payload: &[u8], timestamp_ns: u64) -> SignedHeaders;
    pub fn address(&self) -> &str;
}

pub struct SignedHeaders {
    pub authorization:        String,   // "Authorization: <base64 sig>"
    pub x_requester_address:  String,   // "X-Requester-Address: <bech32>"
    pub x_timestamp:          String,   // "X-Timestamp: <unix ns>"
}
```

The `sign` method takes `&self` (not `&mut self`) — concurrent callers hold no
lock. `timestamp_ns` is supplied by the caller, not sampled inside `sign`, so
the caller controls freshness on each retry.

### Signing Algorithm (5 Steps)

1. **SHA-256 payload hash**: `hash_hex = hex::encode(Sha256::digest(payload))`
2. **Message**: `msg = hash_hex + timestamp_ns.to_string() + transfer_address`
3. **SHA-256 message digest**: `digest = Sha256::digest(msg.as_bytes())`
4. **ECDSA sign**: `(r, s) = SigningKey::sign_prehash_recoverable(&digest)` using k256; take raw 32-byte `r` and 32-byte `s`
5. **Base64 signature**: `sig = base64::encode_config(r || s, STANDARD_NO_PAD)` (64 raw bytes, no padding)

Headers set after signing:
- `Authorization: <sig>`
- `X-Requester-Address: <bech32 address>`
- `X-Timestamp: <timestamp_ns>`

### Address Derivation

1. Compress the secp256k1 public key to 33 bytes
2. `SHA-256(compressed_pubkey)` → 32 bytes
3. `RIPEMD-160` of the SHA-256 output → 20 bytes
4. `bech32::encode(chain_prefix, 20-byte payload, Variant::Bech32)` → address string

---

## 5. EndpointPool

### Design

```rust
pub struct EndpointPool {
    nodes:       Vec<GonkaNode>,          // from config gonka_nodes
    cursor:      AtomicUsize,             // round-robin pointer (wrapping)
    failed_until: Vec<Mutex<Instant>>,    // per-node backoff timestamp
}
```

### Fail-Skip Semantics

- `next_endpoint()` returns the URL of the next available node using round-robin
- A node is "unavailable" if `Instant::now() < failed_until[i]`; skip it and try the next
- `mark_failed(url)` sets `failed_until[i] = Instant::now() + backoff_duration` (default: 30 s)
- If all nodes are unavailable, `next_endpoint()` returns the least-recently-failed node rather than erroring immediately — callers should check the final HTTP response
- If the pool has only one node, fail-skip still applies; exhaustion is reported via `LlmError::AllEndpointsFailed`

---

## 6. `send_signed_with_retry` Contract

```rust
async fn send_signed_with_retry(
    pool:    &EndpointPool,
    signer:  &RequestSigner,
    body:    Vec<u8>,          // byte-identical to what was serialised
    config:  &GonkaRetryConfig,
) -> Result<Bytes, LlmError>
```

**Contract:**
1. `timestamp_ns = system_time_ns()` — sampled fresh at the start of every attempt
2. `headers = signer.sign(&body, timestamp_ns)` — signed with the fresh timestamp
3. Wrap in `tokio::time::timeout(config.request_timeout, http_send(url, headers, body))`
4. On `5xx` or timeout: `pool.mark_failed(url)`, get next URL, **go to step 1** (re-sign with new timestamp)
5. On `401`: return `LlmError::AuthenticationFailed` immediately — do not retry
6. On success: return response bytes
7. After `config.max_retries` attempts without success: return `LlmError::AllEndpointsFailed`

> [!danger]
> Reusing a `timestamp_ns` across retries causes the network to reject the
> request as a replay. Step 1 MUST always sample a fresh timestamp.

---

## 7. `LlmProvider` Method Table

| Method | Behaviour |
|--------|-----------|
| `chat` | Build body via inner `OpenAiProvider`; sign; send; decode |
| `chat_stream` | As `chat` but request SSE stream; fall back to `chat` if unsupported |
| `chat_with_tools` | Build tools-enabled body; sign; send; decode tool call response |
| `embed` | Return `LlmError::Unsupported` — gonka network is text-generation-only at launch |
| `supports_streaming` | `true` |
| `supports_embeddings` | `false` |
| `supports_tool_use` | `true` |
| `supports_vision` | `false` |
| `debug_request_json` | Delegate to inner `OpenAiProvider::debug_request_json` |
| `name` | Provider name from config (e.g., `"gonka"`) |
| `last_usage` | Parsed from `usage` field in OpenAI-format response |

---

## 8. Config Schema Additions

### New `ProviderKind` variant

```toml
[[llm.providers]]
name               = "gonka"
type               = "gonka"               # new ProviderKind::Gonka
model              = "gonka/llama-3.1-8b"
gonka_chain_prefix = "gonka"               # default; "cosmos" on testnet
request_timeout    = "30s"                 # per-request timeout
max_retries        = 3

[[llm.providers.gonka_nodes]]
url  = "https://node1.gonka.ai"
name = "node1"                             # optional, for doctor output

[[llm.providers.gonka_nodes]]
url  = "https://node2.gonka.ai"
name = "node2"
```

### New config types (in `crates/zeph-config/src/llm.rs`)

```rust
pub struct GonkaNode {
    pub url:  String,
    pub name: Option<String>,
}

// Added to ProviderEntry:
pub gonka_nodes:        Vec<GonkaNode>,
pub gonka_chain_prefix: Option<String>,   // default "gonka"
pub request_timeout:    Option<Duration>, // default 30 s
pub max_retries:        Option<u8>,       // default 3
```

---

## 9. Vault Secrets Resolution

| Vault Key | Usage |
|-----------|-------|
| `ZEPH_GONKA_PRIVATE_KEY` | Raw private key bytes (hex or base64); loaded once at startup, zeroized after `RequestSigner::new()` |
| `ZEPH_GONKA_ADDRESS` | Pre-computed bech32 address; used as cross-check against derived address; printed in `doctor` |

At startup, `GonkaProvider::new()`:
1. Reads `ZEPH_GONKA_PRIVATE_KEY` from the age vault
2. Calls `RequestSigner::new(key_bytes, chain_prefix)`
3. Validates derived address against `ZEPH_GONKA_ADDRESS` (if set); logs a warning on mismatch
4. Zeroizes key bytes immediately after signer construction

> [!danger]
> `ZEPH_GONKA_PRIVATE_KEY` bytes MUST be zeroized via `zeroize::Zeroize::zeroize()`
> immediately after `RequestSigner::new()` returns, even on error.

---

## 10. Wizard Branch (Native Setup)

`src/init/llm.rs` — new branch "Gonka.ai (native network)":

1. **Prerequisite check**: verify `inferenced` CLI is on PATH; if not, print install URL and exit wizard branch
2. **Key export guidance**: display `inferenced export-key` command; prompt user to paste the exported key (hex or base64)
3. **Vault storage**: store key as `ZEPH_GONKA_PRIVATE_KEY` in age vault; display derived bech32 address; ask user to confirm it matches their gonka address
4. **Address storage**: store derived address as `ZEPH_GONKA_ADDRESS` in age vault
5. **Node configuration**: prompt for at least one node URL (default: `https://node1.gonka.ai`); allow adding more
6. **Self-test**: attempt a minimal signed request (`POST /v1/chat/completions`) with a one-token prompt; report success or HTTP error
7. **Config write**: write `[[llm.providers]]` stanza to `config.toml`

---

## 11. `zeph gonka doctor` Diagnostic Command

> [!note]
> This command is a Phase 2 follow-up. It MUST be implemented as part of the
> same PR as `GonkaProvider`, not deferred.

**Location**: `src/cli/gonka.rs` as a subcommand under `zeph gonka`.

**Checks performed** (printed as a pass/fail table):

| Check | Pass condition |
|-------|---------------|
| Vault key `ZEPH_GONKA_PRIVATE_KEY` | Present and parseable |
| Address derivation | Derived address matches `ZEPH_GONKA_ADDRESS` (or `ZEPH_GONKA_ADDRESS` absent and derivation succeeds) |
| Configured nodes | At least one `gonka_nodes` entry |
| Node reachability | HTTP GET to each node URL returns a response (any status) within `request_timeout` |
| Signing round-trip | Minimal signed POST to the first reachable node succeeds (HTTP 200) |
| Chain prefix | `gonka_chain_prefix` is non-empty |

Output example:

```
zeph gonka doctor
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Vault key ZEPH_GONKA_PRIVATE_KEY   ✓ present
Address derivation                 ✓ gonka1abc…xyz
Configured nodes                   ✓ 2 nodes
Node reachability  node1            ✓ 142 ms
Node reachability  node2            ✗ timeout (30 s)
Signing round-trip                 ✓ 210 ms
Chain prefix                       ✓ "gonka"
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Result: 6/7 checks passed
```

Exit code: 0 if all checks pass, 1 otherwise.

---

## 12. Key Invariants

### Always

- Every `.await` against a gonka node is wrapped in `tokio::time::timeout(request_timeout, …)`
- `RequestSigner::sign` is `&self` (lock-free); no mutex is held during signing
- Each retry in `send_signed_with_retry` samples a **fresh** `timestamp_ns` before re-signing
- The bytes passed to `sign()` and to `reqwest::Body::from(Vec<u8>)` are **byte-identical** (no intermediate re-serialisation)
- `ZEPH_GONKA_PRIVATE_KEY` bytes are zeroized immediately after `RequestSigner::new()`, even on error
- Log and TUI output shows only the bech32 address — never the private key or raw key bytes
- Tracing spans are present for: `llm.gonka.request`, `llm.gonka.sign`, `llm.gonka.endpoint.next`

### Ask First

- Changing the signing algorithm (hash function, byte order, encoding) requires cross-checking against the Python reference implementation and updating this spec
- Changing the bech32 derivation path (e.g., adding BIP32 derivation) requires architectural review
- Extending `EndpointPool` backoff parameters beyond the configured `gonka.max_retries` requires spec update

### Never

- Echo `ZEPH_GONKA_PRIVATE_KEY` bytes in any log, TUI panel, error message, or debug dump
- Use `ZEPH_VAULT_BACKEND=env` or shell env vars for gonka secrets — age vault only
- Reuse a `timestamp_ns` across retry attempts
- Skip the `tokio::time::timeout` wrapper on any network call to a gonka node
- Implement a native BIP39/BIP32 wallet — use `inferenced` for key management

---

## 13. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| All nodes timeout simultaneously | After `max_retries` exhausted: `LlmError::AllEndpointsFailed` with per-node error details |
| HTTP 401 from any node | `LlmError::AuthenticationFailed` immediately; no other nodes tried |
| `ZEPH_GONKA_PRIVATE_KEY` missing at startup | `GonkaProvider::new()` returns `Err`; startup aborts with actionable vault error |
| Derived address mismatches `ZEPH_GONKA_ADDRESS` | Log `WARN`; continue (address in vault may be stale); doctor reports mismatch |
| Key bytes fail to parse as secp256k1 scalar | `SignerError::InvalidKey`; startup aborts |
| Single-node pool with that node failing | `mark_failed`; next `next_endpoint()` returns same node after backoff elapsed; retry at most `max_retries` times |
| Clock skew > 60 s on CI VM causing 401s | Out-of-scope to fix in Zeph; `zeph gonka doctor` reports signing round-trip failure; user must sync system clock |
| `inferenced` CLI not on PATH during wizard | Wizard prints install URL and exits native setup branch gracefully; GonkaGate branch remains available |
| Node returns malformed JSON | `LlmError::ParseError` with raw bytes included in debug log at `TRACE` level |
| Body exceeds node's size limit (HTTP 413) | Non-retryable `LlmError::RequestTooLarge`; surface to caller |

---

## 14. Risks and Mitigations

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| Signing scheme drifts from Python reference (timestamp source, encoding) | High — all requests rejected | High | Pin test vectors from Python ref impl in `signer_tests.rs`; diff on every protocol update |
| Body canonicalisation divergence (re-serialisation changes bytes) | High — 401 or bad response | Medium | Use `reqwest::Body::from(body.clone())` immediately after signing; single codepath |
| Clock skew on CI VMs causing 401s | Medium — flaky CI | Medium | Skip signing round-trip test in CI unless `GONKA_TEST_NODES` env var is set |
| `inferenced` CLI breaking changes | Medium — wizard broken | Medium | Wizard detects CLI version; prints upgrade instructions; does not block non-wizard path |
| bech32 prefix differs on testnet (`cosmos` vs `gonka`) | Low — address mismatch | Low | `gonka_chain_prefix` is configurable; default `"gonka"`; doctor check validates prefix |

---

## 15. Acceptance Criteria

### Unit / Integration Tests

- [ ] `signer_tests.rs`: fixed test vector (known private key → known signature for known payload and timestamp) matches expected base64 output
- [ ] `signer_tests.rs`: address derivation from known private key matches expected bech32 address
- [ ] `pool_tests.rs`: round-robin returns nodes in order; `mark_failed` causes the failed node to be skipped until backoff elapses
- [ ] `pool_tests.rs`: all-nodes-failed path returns least-recently-failed node on next call
- [ ] `transport_tests.rs` (wiremock): signed request with correct headers arrives at mock server; retry with fresh timestamp on 5xx; no retry on 401
- [ ] `provider_tests.rs`: `embed()` returns `LlmError::Unsupported`

### Live Testnet (Optional, Gated by `GONKA_TEST_NODES`)

- [ ] `cargo test --features integration -- gonka::live_probe` connects to configured testnet node, sends a minimal one-token completion, and receives HTTP 200

### Pre-PR Checks

- [ ] `cargo +nightly fmt --check` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes with zero new warnings
- [ ] `cargo nextest run --workspace --lib --bins` passes
- [ ] No `ZEPH_GONKA_PRIVATE_KEY` value appears in any log output at any level during test runs
- [ ] `zeph gonka doctor` exits with code 0 when all checks pass on a valid config

---

## 16. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[003-llm-providers/spec]] — `LlmProvider` trait and `AnyProvider` enum
- [[022-config-simplification/spec]] — `[[llm.providers]]` canonical format and `ProviderEntry`
- [[038-vault/spec]] — Age vault backend, zeroize-on-drop guarantee
- [[051-gonka-gateway/spec]] — Phase 1: GonkaGate hosted gateway (zero new Rust code)
