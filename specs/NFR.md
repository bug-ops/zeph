---
aliases:
  - "Zeph NFR"
  - "Zeph Quality Requirements"
tags:
  - nfr
  - requirements/non-functional
  - ai-agent
  - rust
  - status/draft
created: 2026-04-13
project: "Zeph"
status: draft
standard: "ISO/IEC 25010:2011"
related:
  - "[[BRD]]"
  - "[[SRS]]"
  - "[[constitution]]"
  - "[[MOC-specs]]"
---

# Zeph: Non-Functional Requirements Specification

> [!abstract]
> Quality attribute requirements for Zeph, based on ISO/IEC 25010:2011.
> Traceable to [[BRD]]. Detailed functional requirements are in [[SRS]].
> Targets reflect a pre-v1.0, single-user / small-team deployment context.

---

## 1. Introduction

### 1.1 Purpose

This document specifies the non-functional (quality) requirements for Zeph.
It complements [[SRS]] which covers functional requirements. Targets here are
the authoritative source for CI pass/fail criteria, architecture decisions, and
acceptance testing related to quality attributes.

### 1.2 Scope

Quality attributes covered and their relevance:

| Characteristic | Relevance |
|---------------|-----------|
| Performance Efficiency | Critical — agent latency and binary size directly affect developer UX |
| Reliability | High — single-user: crashes must not lose data; Qdrant/MCP absence must degrade gracefully |
| Security | Critical — vault, secrets, SSRF, injection defense, PII |
| Maintainability | High — 24-crate workspace, pre-v1.0 rapid iteration |
| Portability | Medium — macOS + Linux; Windows out of scope |
| Usability | Medium — CLI ergonomics and TUI responsiveness |
| Compatibility | High — MCP, A2A, ACP, Ollama, OpenAI protocol compliance |
| Safety | High — no data loss on crash; audit trail |

### 1.3 Definitions

| Term | Definition |
|------|-----------|
| P95 | 95th percentile latency |
| P99 | 99th percentile latency |
| RTO | Recovery Time Objective — time to restore service after failure |
| RPO | Recovery Point Objective — maximum data loss tolerated |
| MSRV | Minimum Supported Rust Version |
| SSRF | Server-Side Request Forgery |
| PII | Personally Identifiable Information |
| BLAKE3 | Cryptographic hash function used for bearer token comparison |
| zeroize | Rust crate that zeroes memory on drop |
| rustls | Pure-Rust TLS implementation used in place of OpenSSL |

### 1.4 References

- [[BRD]] — Business Requirements Document
- [[SRS]] — Software Requirements Specification
- [[constitution]] — Project-wide non-negotiable principles
- [[038-vault/spec]] — Vault and secret management
- [[010-security/spec]] — Security framework
- [[035-profiling/spec]] — Profiling and tracing
- [[036-prometheus-metrics/spec]] — Prometheus metrics
- ISO/IEC 25010:2011 — Systems and software Quality Requirements and Evaluation

### 1.5 Priority and Trade-offs

> [!tip] Quality Attribute Priority
> When quality attributes conflict, prioritise in this order:
> 1. **Security** — secrets must never leak; injection defenses must not be bypassed
> 2. **Reliability** — no data loss; graceful degradation
> 3. **Performance Efficiency** — agent latency and binary size
> 4. **Maintainability** — codebase health for a fast-moving pre-v1.0 project
> 5. **Usability** — developer ergonomics (TUI, CLI, Telegram UX)
> 6. **Compatibility** — protocol compliance with MCP, A2A, ACP, OpenAI
> 7. **Portability** — macOS + Linux; Windows is explicitly out of scope

---

## 2. Performance Efficiency

### 2.1 Time Behaviour

| ID | Requirement | Target | Measurement | Conditions |
|----|------------|--------|-------------|-----------|
| NFR-PERF-001 | Agent turn overhead (excluding LLM latency) | < 50ms added latency at P95 | Span trace (Tier 1 local chrome) | Single-user session, SQLite memory, no MCP |
| NFR-PERF-002 | Skill hot-reload latency | ≤ 500ms from file change to registry update | `notify` debounce timer in unit test | Directory with ≤ 100 SKILL.md files |
| NFR-PERF-003 | SQLite write per turn | < 50ms | nextest integration test | Local disk, single session |
| NFR-PERF-004 | Qdrant vector insert per turn | < 200ms | nextest integration test | Local Qdrant, 1 000-dim embedding |
| NFR-PERF-005 | TUI frame render time | < 33ms (≥ 30 fps) | ratatui benchmark | During background LLM inference |
| NFR-PERF-006 | Skill matching latency (BM25 + embedding) | < 100ms at P95 | Unit benchmark | Registry ≤ 200 skills |
| NFR-PERF-007 | Config hot-reload after file change | < 2 s from file write to active config | Integration test | Standard config file size |

### 2.2 Resource Utilization

| ID | Requirement | Target | Measurement |
|----|------------|--------|-------------|
| NFR-PERF-010 | Release binary size | ≤ 15 MiB | `ls -la target/release/zeph` |
| NFR-PERF-011 | Idle CPU usage | < 1% | `top` / system metrics during idle TUI |
| NFR-PERF-012 | Baseline RSS (idle session, no Qdrant) | < 80 MiB | `/proc/<pid>/status` or `ps` |
| NFR-PERF-013 | Heap allocations per agent turn (excluding LLM payload) | No unbounded allocations in hot loop | `#[global_allocator]` tracking in profiling build |
| NFR-PERF-014 | No blocking I/O in async hot paths | Zero `std::thread::sleep` or blocking calls inside `.await` chains | Clippy lint `clippy::blocking_in_async` + code review |

### 2.3 Capacity

| ID | Requirement | Target |
|----|------------|--------|
| NFR-PERF-020 | Maximum conversation history per session (SQLite) | Unlimited (never deleted); compaction handles context pressure |
| NFR-PERF-021 | Qdrant index capacity | Limited only by Qdrant configuration; Zeph imposes no artificial cap |
| NFR-PERF-022 | Concurrent MCP server connections | ≥ 10 simultaneous servers per session |
| NFR-PERF-023 | Skill registry size | ≥ 500 SKILL.md files without degrading match latency beyond NFR-PERF-006 |

---

## 3. Reliability

### 3.1 Availability

| ID | Requirement | Target | Measurement |
|----|------------|--------|-------------|
| NFR-REL-001 | Single-session uptime | No agent crashes during a normal session of up to 8 hours | Manual live testing session |
| NFR-REL-002 | Graceful degradation without Qdrant | Agent starts and operates with SQLite-only memory | Integration test: start with `[memory.qdrant] enabled = false` |
| NFR-REL-003 | Graceful degradation without configured MCP servers | Agent starts and operates without any MCP tools | Integration test: misconfigured MCP endpoint |
| NFR-REL-004 | Recovery after LLM provider error | Cascade fallback or logged error; agent turn continues or aborts gracefully (no panic) | Integration test: kill primary provider mid-session |

> [!note] Not Applicable — SLA / Uptime Percentage
> Zeph is a single-user / small-team local agent, not a hosted SaaS.
> Traditional 99.9% uptime SLAs do not apply in pre-v1.0.

### 3.2 Fault Tolerance

| ID | Requirement | Behaviour |
|----|------------|-----------|
| NFR-REL-010 | LLM provider 5xx or timeout | Cascade to next provider if routing strategy supports it; else log error and return user-visible error message |
| NFR-REL-011 | MCP server crash or unavailability | Log failure; remove server's tools from registry; agent continues without them |
| NFR-REL-012 | Skill file parse error | Log error with file path and line; skip the malformed skill; continue loading others |
| NFR-REL-013 | RuntimeLayer hook panic | Hook error is caught; agent turn continues normally |
| NFR-REL-014 | Vault absence or decryption failure | Providers requiring missing secrets are skipped with a logged warning; agent may still start with the remaining providers |
| NFR-REL-015 | Admission control scoring failure | Fail-open: content is admitted; error logged but not propagated |

### 3.3 Recoverability

| ID | Requirement | Target |
|----|------------|--------|
| NFR-REL-020 | Recovery from crash (restart) | Agent restarts and resumes with SQLite history intact; no conversation data lost |
| NFR-REL-021 | RPO (data loss on crash) | Zero message loss: messages are written to SQLite before the LLM is called |
| NFR-REL-022 | RTO (restart time) | < 5 s from process start to accepting the first user message |
| NFR-REL-023 | Config reload without dropping in-flight requests | In-flight LLM calls complete before the new config is applied |

---

## 4. Security

### 4.1 Confidentiality

| ID | Requirement | Implementation |
|----|------------|---------------|
| NFR-SEC-001 | Secrets at rest | All `ZEPH_*` keys encrypted with age (X25519 or scrypt); no plaintext keys on disk |
| NFR-SEC-002 | Secrets in transit | All outbound API traffic via TLS 1.2+ (rustls); `openssl-sys` banned |
| NFR-SEC-003 | Secret memory lifecycle | `Secret<T>` values zeroized on drop via `zeroize` crate; verified in unit tests |
| NFR-SEC-004 | PII in conversation | PII is detected via NER classifier and redacted before injection into LLM context or memory |
| NFR-SEC-005 | Credential env-var scrubbing | `ZEPH_*` and known credential vars are removed from subprocess environments before shell execution |
| NFR-SEC-006 | Debug log redaction | `Secret<T>` renders as `[REDACTED]` in `Debug` output; no key material in logs |

### 4.2 Authentication and Authorization

| ID | Requirement | Implementation |
|----|------------|---------------|
| NFR-SEC-010 | HTTP gateway bearer token | BLAKE3 + constant-time comparison (`subtle` crate); timing-safe |
| NFR-SEC-011 | A2A invocation tokens | IBCT tokens signed with HMAC-SHA256; `key_id` included for rotation |
| NFR-SEC-012 | Subagent tool permissions | `FilteredToolExecutor` enforces `PermissionGrants` with TTL; no tool outside the granted set is accessible |
| NFR-SEC-013 | MCP OAP authorization | Per-server `[tools.authorization]` policy gates MCP tool calls before dispatch |

### 4.3 Integrity

| ID | Requirement | Implementation |
|----|------------|---------------|
| NFR-SEC-020 | Input validation | All external input (user messages, tool output, MCP responses) validated and sanitized at system boundaries |
| NFR-SEC-021 | Injection defense | Eight-layer sanitizer pipeline (spotlighting, regex, NER, guardrail, quarantined summarizer, response verification, exfiltration guard, memory validation) |
| NFR-SEC-022 | SSRF protection | Private IP ranges (RFC 1918, loopback, link-local) rejected; redirect chains validated |
| NFR-SEC-023 | Shell blocklist | Blocklist check runs unconditionally before `PermissionPolicy`; cannot be bypassed |
| NFR-SEC-024 | Symlink boundary | File-loading paths enforce symlink boundary checks; no path traversal |
| NFR-SEC-025 | Tool audit trail | Every tool call logged with `claim_source`, tool name, timestamp |
| NFR-SEC-026 | MCP tool input scan | MCP tool argument schemas scanned for injection patterns before dispatch |

### 4.4 Compliance

> [!note] Not Applicable — Regulatory Compliance
> Zeph is open-source software without a commercial SaaS offering. GDPR, HIPAA,
> PCI DSS, and SOC 2 formal compliance are not applicable in pre-v1.0.
> Users deploying Zeph as a service are responsible for their own compliance obligations.

---

## 5. Usability

### 5.1 Learnability

| ID | Requirement | Target |
|----|------------|--------|
| NFR-USE-001 | Time to first productive CLI session | A developer who has installed the binary and configured one provider completes a first useful conversation in < 5 minutes |
| NFR-USE-002 | Slash command discoverability | `/help` lists all available commands with a one-line description each |
| NFR-USE-003 | Error message quality | All user-facing errors are actionable: state what went wrong and what the user can do to fix it |

### 5.2 Operability

| ID | Requirement | Target |
|----|------------|--------|
| NFR-USE-010 | Background operation visibility in TUI | 100% of background operations (LLM inference, memory search, tool execution, MCP connection, skill reload) show a spinner with a status message |
| NFR-USE-011 | TUI slash autocomplete | Typing `/` in TUI Insert mode shows an autocomplete dropdown populated within one keypress |
| NFR-USE-012 | Config migration | `zeph --migrate-config` upgrades a config from a previous minor version without data loss and without manual editing |
| NFR-USE-013 | Init wizard coverage | `zeph --init` asks about all user-configurable features; no undocumented option requires manual TOML editing for initial setup |

### 5.3 Accessibility

> [!note] Not Applicable — WCAG / Section 508
> Zeph is a terminal application. Web accessibility standards (WCAG 2.1) do not
> apply. Keyboard-only operation is the primary interaction model by design.

### 5.4 Internationalization

| ID | Requirement | Details |
|----|------------|---------|
| NFR-USE-030 | Self-learning feedback language support | `FeedbackDetector` recognises positive/negative signals in at least English, Russian, and German |
| NFR-USE-031 | Configuration language | Configuration file keys and CLI flags are English-only |

---

## 6. Compatibility

### 6.1 Interoperability

| ID | Requirement | Standard/Protocol |
|----|------------|-------------------|
| NFR-COM-001 | OpenAI API compatibility | Zeph's `compatible` provider passes the OpenAI chat completions API schema; tested against known OpenAI-compatible endpoints (LM Studio, vLLM) |
| NFR-COM-002 | MCP protocol compliance | MCP client implements the Model Context Protocol as specified; `rmcp` crate is the wire implementation |
| NFR-COM-003 | A2A protocol compliance | A2A client/server implements JSON-RPC 2.0 with IBCT token signing |
| NFR-COM-004 | ACP protocol compliance | ACP transport implements `agent-client-protocol 0.10.3` |
| NFR-COM-005 | Prometheus / OpenMetrics | `/metrics` endpoint emits valid OpenMetrics 1.0.0 format; scrape-compatible with Prometheus 2.x |
| NFR-COM-006 | OTLP trace export | Tier 2 traces exported in OTLP format compatible with Jaeger 1.x and OpenTelemetry Collector |

### 6.2 Co-existence

| ID | Requirement | Details |
|----|------------|---------|
| NFR-COM-010 | OS support | macOS 13+ (x86_64, aarch64), Linux with glibc 2.31+ (x86_64, aarch64) |
| NFR-COM-011 | Terminal compatibility | CLI channel: any POSIX terminal. TUI: xterm-compatible, 256-colour or truecolour |
| NFR-COM-012 | Ollama version compatibility | Tested against Ollama 0.3+ (current API; `/api/chat` endpoint) |
| NFR-COM-013 | Qdrant version compatibility | Qdrant 1.x; gRPC and HTTP REST interfaces |
| NFR-COM-014 | SQLite version compatibility | sqlx 0.8 with bundled SQLite; no external SQLite installation required |
| NFR-COM-015 | PostgreSQL version compatibility (optional) | PostgreSQL 15+ when `postgres` feature is enabled |

---

## 7. Maintainability

### 7.1 Modularity and Modifiability

| ID | Requirement | Details |
|----|------------|---------|
| NFR-MNT-001 | Workspace structure | 24-crate Cargo workspace with a strict 5-layer DAG; same-layer imports prohibited |
| NFR-MNT-002 | Feature flag discipline | `default = []`; optional features declared with `dep:zeph-<name>`; no optional feature enabled by default |
| NFR-MNT-003 | Dependency additions | New external dependencies require version check via context7 MCP and explicit justification in PR description |
| NFR-MNT-004 | No backward-compatibility shims before v1.0 | Breaking changes documented in `CHANGELOG.md`; no `#[deprecated]` shims required before v1.0 |
| NFR-MNT-005 | No unsafe code | `unsafe_code = "deny"` in workspace `Cargo.toml`; verified by CI |

### 7.2 Testability

| ID | Requirement | Details |
|----|------------|---------|
| NFR-MNT-010 | Pre-merge test pass | `cargo nextest run --config-file .github/nextest.toml --workspace --features full --lib --bins` passes with zero failures on every PR |
| NFR-MNT-011 | Formatting check | `cargo +nightly fmt --check` passes on every PR |
| NFR-MNT-012 | Lint check | `cargo clippy --all-targets --all-features --workspace -- -D warnings` passes on every PR |
| NFR-MNT-013 | Doc-test coverage | `cargo test --doc --workspace --features "desktop,ide,server,chat,pdf,scheduler"` passes for all touched crates |
| NFR-MNT-014 | LLM serialization gate | Any PR touching LLM request/response serialization paths requires a live API session test with debug dump verification |
| NFR-MNT-015 | Integration tests | `cargo nextest run -- --ignored` covers Qdrant-dependent tests; run in CI with Qdrant service |
| NFR-MNT-016 | Unit test patterns | Unit tests use `MockProvider` and `MockChannel` patterns; no real network calls in unit tests |

### 7.3 Analysability

| ID | Requirement | Details |
|----|------------|---------|
| NFR-MNT-020 | Structured logging | All log calls use the `tracing` crate; `log` crate is banned |
| NFR-MNT-021 | API documentation | Every `pub` type, trait, function, and method has a `///` doc comment explaining what and why |
| NFR-MNT-022 | Doc build verification | `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p <crate>` passes for every touched crate |
| NFR-MNT-023 | Debug request visibility | `LlmProvider::debug_request_json()` returns the exact JSON payload sent to the API; used in debug dumps |
| NFR-MNT-024 | Two-tier trace instrumentation | Tier 1 (local Chrome JSON): zero-overhead when disabled; Tier 2 (OTLP): gated by `profiling` feature |
| NFR-MNT-025 | Metrics snapshot | ~25 gauge/counter metrics available in `MetricsSnapshot`; exported to TUI and Prometheus |
| NFR-MNT-026 | CHANGELOG maintenance | `CHANGELOG.md` `[Unreleased]` section updated at the end of every implementation phase |

---

## 8. Portability

### 8.1 Adaptability

| ID | Requirement | Details |
|----|------------|---------|
| NFR-POR-001 | Supported platforms | macOS 13+ and Linux (glibc 2.31+); x86_64 and aarch64 |
| NFR-POR-002 | No containerization requirement | Zeph runs as a native binary; Docker is not required; Docker may be used optionally for Qdrant |
| NFR-POR-003 | No cloud lock-in | Provider abstraction allows any combination of local (Ollama, candle) and cloud (Claude, OpenAI) providers |
| NFR-POR-004 | Cross-compilation | The workspace supports cross-compilation to aarch64 targets via `cross` or standard cargo targets |

### 8.2 Installability

| ID | Requirement | Details |
|----|------------|---------|
| NFR-POR-010 | Installation method | Single binary distribution; `cargo install` from source or pre-built GitHub releases |
| NFR-POR-011 | Configuration | TOML config file with `ZEPH_*` env-var overrides; `--init` wizard for first-time setup |
| NFR-POR-012 | No system-level dependencies | No dynamic library dependencies beyond the system libc; all TLS via rustls (statically linked) |

> [!note] Windows — Explicitly Out of Scope
> Windows is not a supported platform for Zeph. This is a deliberate business
> constraint documented in [[BRD#Out of Scope]]. No portability targets for
> Windows are required.

---

## 9. Safety

> [!note]
> ISO 25010 does not define "Safety" as a primary characteristic, but the term
> is used here to capture operational safety properties: no data loss on crash,
> graceful shutdown, and audit logging.

| ID | Requirement | Target | Verification |
|----|------------|--------|--------------|
| NFR-SAF-001 | No message data loss on crash | Messages written to SQLite before LLM is called; RPO = 0 messages | Integration test: kill -9 the agent process mid-turn; confirm prior messages intact |
| NFR-SAF-002 | Graceful shutdown on SIGTERM | Agent completes the in-flight LLM call (or times out after 10 s) and flushes pending SQLite writes before exiting | Signal handler test |
| NFR-SAF-003 | Immutable audit trail | Tool call audit log (JSONL) is append-only; no modification API exposed | Code review of audit writer |
| NFR-SAF-004 | No panic in library crates | `panic!()` is forbidden in `zeph-*` library crates except in tests and unreachable branches | `cargo clippy -- -W clippy::panic` in CI |
| NFR-SAF-005 | Vault key zeroize on drop | `Secret<T>` memory is zeroed on drop | Unit test using address sanitizer / valgrind |
| NFR-SAF-006 | No credentials in panic messages | Error types derived with `thiserror` must not include secret field values in `Display` | Code review; `Secret<T>` redacted `Debug` impl |

---

## 10. Verification Matrix

| ID | Method | Environment | Frequency |
|----|--------|-------------|-----------|
| NFR-PERF-001 | Span trace analysis (Tier 1 chrome JSON) | Local dev machine | Per feature PR |
| NFR-PERF-002 | `notify` debounce unit test | CI | Every commit |
| NFR-PERF-010 | `ls -la target/release/zeph` | CI release build | Per release |
| NFR-PERF-011 | `top` / system metrics | Manual | Per milestone |
| NFR-REL-001 | 8-hour live session test | Developer machine | Per release |
| NFR-REL-002 | nextest integration test | CI | Every commit |
| NFR-REL-020 | nextest integration test | CI | Every commit |
| NFR-SEC-001 | Vault inspection (`zeph vault get`) | Local | Config review |
| NFR-SEC-003 | Unit test with address sanitizer | CI (nightly) | Per release |
| NFR-SEC-010 | Unit test (constant-time comparison) | CI | Every commit |
| NFR-SEC-022 | Unit test (SSRF rejection) | CI | Every commit |
| NFR-MNT-010 | `cargo nextest run` | CI | Every commit |
| NFR-MNT-011 | `cargo +nightly fmt --check` | CI | Every commit |
| NFR-MNT-012 | `cargo clippy -- -D warnings` | CI | Every commit |
| NFR-MNT-013 | `cargo test --doc` | CI | Every PR |
| NFR-MNT-022 | `cargo doc --no-deps` | CI | Every PR |
| NFR-POR-001 | GitHub Actions matrix (macOS + Linux) | CI | Every commit |
| NFR-SAF-001 | Kill-9 integration test | Local | Per milestone |
| NFR-SAF-002 | SIGTERM integration test | CI | Per release |
| NFR-SAF-004 | `cargo clippy -- -W clippy::panic` | CI | Every commit |

---

## 11. Open Questions

> [!question] Unresolved Quality Requirements
>
> - [ ] Should a formal memory / resource limit be enforced per session (e.g.,
>       max SQLite DB size) before v1.0?
> - [ ] What is the P99 latency budget for MCP tool discovery at session start
>       with 10+ servers?
> - [ ] Should the binary size limit (15 MiB) apply to the `full` feature set
>       or to the default build only?
> - [ ] Is there a plan for fuzz testing on the sanitizer pipeline or config
>       parser before v1.0?

---

## See Also

- [[BRD]] — business requirements (source)
- [[SRS]] — functional requirements
- [[constitution]] — project-wide non-negotiable principles
- [[MOC-specs]] — index of all specifications
- [[010-security/spec]] — security framework detail
- [[035-profiling/spec]] — profiling and tracing detail
- [[036-prometheus-metrics/spec]] — Prometheus metrics detail
