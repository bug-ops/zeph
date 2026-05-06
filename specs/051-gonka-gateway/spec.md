---
aliases:
  - GonkaGate Gateway
  - Gonka Phase 1
  - GonkaGate
tags:
  - sdd
  - spec
  - llm
  - providers
  - config
created: 2026-05-05
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[022-config-simplification/spec]]"
  - "[[038-vault/spec]]"
  - "[[052-gonka-native/spec]]"
---

# Spec: Gonka.ai Gateway Integration (Phase 1)

> [!info]
> Phase 1 integration of gonka.ai inference via the GonkaGate hosted gateway.
> Uses the existing `CompatibleProvider` with zero new Rust code. The gateway
> presents a standard OpenAI-compatible Chat Completions API, so Zeph connects
> by pointing a `compatible` provider entry at `https://api.gonkagate.com/v1`
> with a `gp-…` Bearer key stored in the age vault.

## Sources

### External
- [Gonka.ai network overview](https://gonka.ai)
- [GonkaGate API gateway](https://api.gonkagate.com)
- [OpenAI Chat Completions API reference](https://platform.openai.com/docs/api-reference/chat)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-llm/src/compatible.rs` | `CompatibleProvider` — OpenAI-compatible HTTP impl reused as-is |
| `src/init/llm.rs` | Interactive LLM wizard (`--init`) where the new GonkaGate branch is added |
| `config/default.toml` | Commented-out example stanza for GonkaGate |
| `crates/zeph-vault/src/` | Age vault backend — vault key resolution at startup |

---

## 1. Overview

### Problem Statement

Gonka.ai is a decentralized AI inference network running on a Cosmos-SDK chain.
Users who hold GNK tokens or trial credits can run inference at lower cost than
centralised cloud providers. Zeph has no integration path today, so these users
must manage a separate tool to access gonka.ai models.

GonkaGate is the officially hosted, zero-setup entry point: it accepts standard
OpenAI-compatible requests and forwards them to the gonka network, abstracting
all blockchain interaction. The Phase 1 goal is to let Zeph users route
inference through GonkaGate with a single config change and no new compiled code.

### Goal

A Zeph user with a `gp-…` GonkaGate API key can add a `compatible` provider
entry pointing at `https://api.gonkagate.com/v1`, restart Zeph, and run
inference through the gonka.ai network with the same experience as any other
`compatible` provider.

### Out of Scope

- Native gonka network transport (secp256k1 signing, bech32 addresses) — see [[052-gonka-native/spec]]
- GNK token purchase or faucet integration
- Model discovery or capability probing for GonkaGate models
- Streaming support beyond what `CompatibleProvider` already provides
- Multi-account or multi-key setups

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a `[[llm.providers]]` entry has `type = "compatible"` and `base_url = "https://api.gonkagate.com/v1"` THE SYSTEM SHALL route chat requests to GonkaGate using the configured Bearer token | must |
| FR-002 | WHEN the interactive wizard (`--init`) is run and the user selects "Gonka.ai (GonkaGate)" THE SYSTEM SHALL prompt for the `gp-…` API key, store it in the age vault as `ZEPH_COMPATIBLE_GONKAGATE_API_KEY`, and write the provider stanza to `config.toml` | must |
| FR-003 | WHEN the wizard writes the GonkaGate provider stanza THE SYSTEM SHALL set `type = "compatible"`, `base_url = "https://api.gonkagate.com/v1"`, and `api_key_env = "ZEPH_COMPATIBLE_GONKAGATE_API_KEY"` | must |
| FR-004 | WHEN `config/default.toml` is shipped THE SYSTEM SHALL include a commented-out GonkaGate example stanza so users can enable it by uncommenting | should |
| FR-005 | WHEN the vault key `ZEPH_COMPATIBLE_GONKAGATE_API_KEY` is absent at startup and GonkaGate is configured THE SYSTEM SHALL emit a clear error message and refuse to start | must |
| FR-006 | WHEN a GonkaGate request fails with HTTP 401 or 403 THE SYSTEM SHALL surface "GonkaGate authentication failed — check ZEPH_COMPATIBLE_GONKAGATE_API_KEY in vault" and not retry | must |
| FR-007 | WHEN the wizard self-test is enabled THE SYSTEM SHALL send a single minimal chat completion request to GonkaGate and report success or failure before writing the config | should |

---

## 3. Architecture

### Design Principle

Phase 1 introduces **zero new Rust code in hot paths**. All inference plumbing
is handled by the existing `CompatibleProvider`. The integration is entirely
configuration-level:

```
User request
    │
    ▼
AnyProvider::Compatible(CompatibleProvider {
    base_url: "https://api.gonkagate.com/v1",
    api_key:  <resolved from vault ZEPH_COMPATIBLE_GONKAGATE_API_KEY>,
    model:    "gonka/<model-name>",
})
    │
    ▼ HTTP POST /v1/chat/completions (Bearer gp-…)
    │
    ▼
GonkaGate proxy → gonka.ai network
```

### Vault Key Convention

The key follows the project-wide naming pattern for `compatible` provider keys:

```
ZEPH_COMPATIBLE_<PROVIDER_SLUG>_API_KEY
```

For GonkaGate specifically: `ZEPH_COMPATIBLE_GONKAGATE_API_KEY`.

### Wizard Touch-Point

`src/init/llm.rs` — add a branch in the LLM provider selection menu:

1. New option: "Gonka.ai (via GonkaGate)"
2. Prompt: "Enter your GonkaGate API key (starts with `gp-`):"
3. Validate prefix (`gp-`) and non-empty
4. Store in age vault: `zeph vault set ZEPH_COMPATIBLE_GONKAGATE_API_KEY <key>`
5. Optional self-test: POST to `https://api.gonkagate.com/v1/chat/completions`
6. Write provider stanza to `config.toml`

### Config Stanza

```toml
# Gonka.ai via GonkaGate hosted gateway
# Get your API key at https://api.gonkagate.com
[[llm.providers]]
name        = "gonkagate"
type        = "compatible"
base_url    = "https://api.gonkagate.com/v1"
model       = "gonka/llama-3.1-8b"          # replace with desired model
api_key_env = "ZEPH_COMPATIBLE_GONKAGATE_API_KEY"
```

---

## 4. Key Invariants

### Always

- The `gp-…` key is resolved exclusively from the age vault — never from env vars or plaintext config
- `api_key_env` in the config stanza contains only the vault key name, not the key value itself
- The wizard validates the `gp-` prefix before writing to the vault
- All log output that references the key shows only the vault key name (`ZEPH_COMPATIBLE_GONKAGATE_API_KEY`) — never the key value

### Ask First

- Changing the vault key name convention from `ZEPH_COMPATIBLE_GONKAGATE_API_KEY` to another name requires updating both the wizard and the default.toml comment
- Adding GonkaGate to the default provider fallback chain (not merely an example stanza) requires architectural review

### Never

- Pass the `gp-…` key via `ZEPH_VAULT_BACKEND=env` or shell environment variables — age vault only
- Echo the key value in any log, TUI status line, or error message
- Hardcode `https://api.gonkagate.com/v1` anywhere other than the wizard default and the default.toml comment — the user's config stanza is the runtime source of truth

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Vault key absent at startup | Startup fails with actionable error: "ZEPH_COMPATIBLE_GONKAGATE_API_KEY not found in vault — run `zeph vault set ZEPH_COMPATIBLE_GONKAGATE_API_KEY <your-key>` or re-run `zeph --init`" |
| HTTP 401 from GonkaGate | Non-retryable `LlmError::AuthenticationFailed`; surface wizard hint in error message |
| HTTP 429 rate limit | Respect `Retry-After` header if present; otherwise apply standard backoff (inherited from `CompatibleProvider`) |
| Network timeout | `CompatibleProvider` timeout config applies; no GonkaGate-specific override required |
| Self-test failure during wizard | Print failure reason, ask "Continue anyway? (y/N)"; abort unless user confirms |
| Model name not found on GonkaGate | GonkaGate returns HTTP 404 or model-not-found error; surface as `LlmError::ModelNotFound` |
| Wizard run with no age vault configured | Wizard prints: "Age vault required — run `zeph --init` with vault setup first" and exits |

---

## 6. Success Criteria

- [ ] Adding a `compatible` provider with `base_url = "https://api.gonkagate.com/v1"` and a valid `gp-…` key in the vault produces a successful chat completion round-trip
- [ ] The `--init` wizard presents a "Gonka.ai (GonkaGate)" option that stores the key in the vault and writes the correct provider stanza
- [ ] `config/default.toml` contains a commented-out GonkaGate stanza that is syntactically valid when uncommented
- [ ] Starting Zeph with a missing `ZEPH_COMPATIBLE_GONKAGATE_API_KEY` emits the expected actionable error and does not start
- [ ] The CLI mode (`cargo run -- chat "hello"`) and TUI mode (`cargo run -- --tui`) both produce a response via GonkaGate with no behaviour difference
- [ ] No `gp-…` key value appears in any log output at any log level
- [ ] `cargo +nightly fmt --check`, `cargo clippy --workspace -- -D warnings`, and `cargo nextest run --workspace --lib --bins` all pass with no regressions after the wizard change

---

## 7. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[003-llm-providers/spec]] — `CompatibleProvider` implementation and provider trait
- [[022-config-simplification/spec]] — `[[llm.providers]]` canonical format
- [[038-vault/spec]] — Age vault backend and key resolution
- [[052-gonka-native/spec]] — Phase 2: native gonka network transport with ECDSA signing
