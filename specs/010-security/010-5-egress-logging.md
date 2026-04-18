---
aliases:
  - Egress Logging
  - Outbound Network Audit
  - EgressEvent
tags:
  - sdd
  - spec
  - security
  - tools
  - audit
  - observability
created: 2026-04-17
status: approved
related:
  - "[[010-security/spec]]"
  - "[[010-4-audit]]"
  - "[[010-3-authorization]]"
  - "[[006-tools/spec]]"
  - "[[039-background-task-supervisor/spec]]"
---

# Spec: Egress Network Logging

> [!info]
> Structured audit of every outbound network call issued by tool executors
> (`WebScrapeExecutor`, `FetchExecutor`, redirect hops, SSRF/domain blocks).
> Emitted as `EgressEvent` records on the existing JSONL audit stream, correlated
> with `AuditEntry` via `correlation_id`, and surfaced to the TUI Security panel.

## Sources

### External
- **OWASP AI Agent Security Cheat Sheet** — egress monitoring guidance: https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html
- **NIST SP 800-53 AU-2** — Audit Events (egress classification).

### Internal
| File | Contents |
|---|---|
| `crates/zeph-tools/src/audit.rs` | `AuditEntry`, `AuditLogger`, `AuditResult` — shared JSONL sink |
| `crates/zeph-tools/src/scrape.rs` | `WebScrapeExecutor`, `fetch_html`, redirect handling, SSRF checks |
| `crates/zeph-tools/src/config.rs` | `ToolsConfig`, `ScrapeConfig` — domain allow/deny lives here |
| `crates/zeph-core/src/metrics.rs` | `MetricsSnapshot`, `SecurityEventCategory`, `bg_dropped` pattern |
| `crates/zeph-core/src/agent/builder.rs` | Executor wiring, supervisor spawn site |
| `crates/zeph-tui/src/widgets/security.rs` | Security panel renderer |

---

## 1. Overview

### Problem Statement

Today the audit logger records successful/blocked/errored tool invocations but not
the individual outbound HTTP calls that tool executors issue. Operators cannot
answer: "which hosts did the agent talk to this turn?", "how many bytes were
exfiltrated per host?", "did a redirect chain resolve through a private IP?",
"how many requests were dropped by the SSRF guard?". Without this signal,
incident investigation relies on reconstructing egress from raw traces and
external proxy logs — neither is guaranteed available.

### Goal

- Emit a structured `EgressEvent` record for every outbound HTTP attempt by a
  tool executor — success, pre-response failure, and pre-flight block alike.
- Correlate each `EgressEvent` with its parent `AuditEntry` via a shared
  `correlation_id` so consumers can walk from tool call to per-hop HTTP detail.
- Surface aggregate counters (total, blocked, bytes, dropped) and the most
  recent events in the TUI Security panel.
- Keep telemetry plumbing bounded: no unbounded channels, no blocking audit
  writes in the tool hot path.

### Out of Scope

- Egress *policy* enforcement — SSRF / domain allowlist / scheme validation
  remain owned by `ScrapeConfig` + `validate_url` + `check_domain_policy`.
  `EgressEvent` is observational; see §4 invariants.
- Intercepting outbound calls made by LLM providers, MCP stdio/subprocess I/O,
  Qdrant/SQLite clients — only tool executors are in scope for v1.
- Adding a `type` discriminator field to `AuditEntry` — see §3.3 (field-presence
  approach, no schema bump).
- Any `domain_allowlist` / `domain_blocklist` duplication under `[tools.egress]`.
  Domain policy keeps single ownership in `[tools.scrape]`.

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a tool executor issues an outbound HTTP request AND `[tools.egress].enabled = true` THE SYSTEM SHALL emit an `EgressEvent` on success OR on pre-response failure. | must |
| FR-002 | WHEN a tool executor rejects a request (SSRF / domain / scheme) AND `[tools.egress].log_blocked = true` THE SYSTEM SHALL emit an `EgressEvent` with `blocked = true` and `block_reason` set before returning an error. | must |
| FR-003 | WHEN an `EgressEvent` is emitted THE SYSTEM SHALL include the `correlation_id` of the parent `AuditEntry` generated at the top of `execute_tool_call`. | must |
| FR-004 | WHEN a fetch follows a redirect chain THE SYSTEM SHALL emit one `EgressEvent` per hop with `hop: 0..N` sharing the same `correlation_id`. | must |
| FR-005 | WHEN the egress telemetry channel is full THE SYSTEM SHALL drop the event, increment `egress_dropped_total`, and continue without stalling the executor. | must |
| FR-006 | WHEN `[tools.egress].log_response_bytes = false` THE SYSTEM SHALL serialize `response_bytes: 0` in the JSONL record. | should |
| FR-007 | WHEN `[tools.egress].log_hosts_to_tui = false` THE SYSTEM SHALL replace the host in `MetricsSnapshot.egress_recent` with `"***"`; the JSONL record keeps the real host. | should |
| FR-008 | WHEN the agent is running in `--tui` mode AND audit destination is `stdout` THE SYSTEM SHALL redirect egress writes to the same `audit.jsonl` file used by `AuditEntry` writes. | must |
| FR-009 | WHEN the session ends THE SYSTEM SHALL flush pending egress events from the channel before the drain task exits. | should |
| FR-010 | WHEN egress events accumulate in `MetricsSnapshot.egress_recent` THE SYSTEM SHALL cap the ring at 20 entries (FIFO eviction). | must |
| FR-011 | WHEN an egress event is blocked THE SYSTEM SHALL push a `SecurityEventCategory::EgressBlocked` entry to the security-event ring. Allowed events SHALL NOT be pushed (counter-only) to avoid flooding. | must |

---

## 3. Architecture

### 3.1 `EgressEvent` type

```rust
/// Outbound network call record emitted by HTTP-capable executors.
/// Serialized as JSON Lines onto the shared audit sink.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressEvent {
    /// Unix timestamp (seconds) when the request was issued.
    pub timestamp: String,
    /// Record-type discriminator on the shared JSONL stream — always `"egress"`.
    /// Consumers distinguish `EgressEvent` from `AuditEntry` by the presence
    /// of this field (field-presence approach; no schema version bump).
    pub kind: &'static str,
    /// Correlation id shared with the parent `AuditEntry` (UUIDv4, lowercased).
    /// Required on every `EgressEvent`.
    pub correlation_id: String,
    /// Tool that issued the call (`"web_scrape"`, `"fetch"`, ...).
    pub tool: ToolName,
    /// Destination URL (after SSRF/domain validation).
    pub url: String,
    /// Hostname — denormalized for TUI aggregation.
    pub host: String,
    /// HTTP method (`"GET"`, `"POST"`, ...).
    pub method: String,
    /// HTTP response status. `None` when the request failed pre-response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Wall-clock duration from send to end-of-body, in milliseconds.
    pub duration_ms: u64,
    /// Bytes of response body received. Zero on pre-response failure or
    /// when `log_response_bytes = false`.
    pub response_bytes: usize,
    /// Whether the request was blocked before connection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
    /// Block reason: `"allowlist"` | `"blocklist"` | `"ssrf"` | `"scheme"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<&'static str>,
    /// Caller identity propagated from `ToolCall::caller_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_id: Option<String>,
    /// Redirect hop index (0 for the initial request); distinguishes per-hop
    /// events sharing the same `correlation_id`.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub hop: u8,
}
```

### 3.2 `AuditEntry::correlation_id` schema change

A new field is added to `AuditEntry`:

```rust
/// Correlation id shared with any associated `EgressEvent` emitted during
/// this tool call. Generated at `execute_tool_call` entry. `None` for
/// policy-only / legacy rollback entries that do not map to a tool call.
#[serde(skip_serializing_if = "Option::is_none")]
pub correlation_id: Option<String>,
```

Consumers distinguish record types by **field presence**:
- `EgressEvent` has `kind: "egress"` (always present).
- `AuditEntry` has no `kind` field (schema unchanged except `correlation_id`).

No `schema_version` bump, no added `type: "audit"` field on `AuditEntry` — this
keeps existing JSONL consumers working while the `kind` field gives a clean
positive signal for egress records.

#### AuditEntry literal edit budget

Adding the `correlation_id` field forces a mechanical update at **~19 call
sites** across the workspace:

| Location | Count | Notes |
|---|---|---|
| `crates/zeph-tools/src/audit.rs` (tests) | ~11 | Unit tests construct literals directly |
| `crates/zeph-tools/src/audit.rs` (new `EgressEvent` round-trip test) | 1 | New |
| `crates/zeph-tools/src/sanitize.rs` | 1 | `AuditEntry { .. }` literal |
| `crates/zeph-core/src/agent/tool_execution/native.rs` | 1 | Tool invocation audit |
| `crates/zeph-tools/src/scrape.rs` | 1 | `log_audit` construction |
| `crates/zeph-tools/src/shell/mod.rs` | 1 | Shell audit path |
| `crates/zeph-tools/src/policy_gate.rs` | 3 | Policy check / block / error paths |
| `crates/zeph-tools/src/adversarial_gate.rs` | 1 | Adversarial policy audit |

Each site gains an explicit `correlation_id: ...` entry. No `..Default::default()`
shortcut — keep literals exhaustive so future fields fail the build loudly.

### 3.3 Bounded egress telemetry channel

The executor pushes `EgressEvent`s to a bounded `tokio::sync::mpsc` channel
(capacity **256**) created by `AgentBuilder`. The drain task is registered as
`TaskClass::Telemetry` in the background supervisor (spec `039`). Send policy:

```rust
match egress_tx.try_send(event) {
    Ok(()) => {}
    Err(TrySendError::Full(_))  => egress_dropped.fetch_add(1, Ordering::Relaxed),
    Err(TrySendError::Closed(_)) => tracing::debug!("egress channel closed"),
}
```

- `try_send` never `.awaits` — keeps the tool hot path non-blocking.
- `egress_dropped` is an `Arc<AtomicU64>` rolled up into
  `MetricsSnapshot::egress_dropped_total` at the same cadence as `bg_dropped`.
- Drain task flushes with `while let Ok(ev) = rx.try_recv()` before returning.

### 3.4 Metrics surface

```rust
/// Outbound network requests emitted by tool executors (all outcomes).
pub egress_requests_total: u64,
/// Outbound requests that were blocked before issuing a connection.
pub egress_blocked_total: u64,
/// Aggregate response bytes across all successful egress calls.
pub egress_bytes_total: u64,
/// Count of `EgressEvent`s dropped due to telemetry channel saturation.
pub egress_dropped_total: u64,
/// Ring buffer of recent events for the TUI Security panel (cap 20, FIFO).
pub egress_recent: std::collections::VecDeque<EgressSnapshot>,
```

`EgressSnapshot` is a trimmed copy surfaced to the TUI (respects
`log_hosts_to_tui`): `{ timestamp, host, status, duration_ms, response_bytes,
blocked, correlation_id: <8-char-prefix> }`.

Two new `SecurityEventCategory` variants:
- `EgressBlocked` — pushed to the ring on every block (rare).
- `EgressAllowed` — counter-only (never pushed, else floods).

### 3.5 Config

```toml
[tools.egress]
enabled = true             # master switch
log_blocked = true         # emit events for pre-flight blocks
log_response_bytes = true  # include response bytes in records
log_hosts_to_tui = true    # show real hostname in TUI; JSONL keeps real host
```

No `domain_allowlist` / `domain_blocklist` fields. Domain policy lives solely
in `[tools.scrape]` (`allowed_domains` / `denied_domains`) — unchanged.

### 3.6 Data flow

```
execute_tool_call
  ├─ correlation_id = UUIDv4
  └─ handle_fetch / scrape_instruction (cid, caller_id)
       ├─ validate_url / check_domain_policy / resolve_and_validate
       │    └─ reject → log_egress(blocked=true, reason, hop=0, cid)
       └─ fetch_html (per hop N: log_egress(status, hop=N, cid))
                 │
                 ├─ JSONL sink (same Mutex<File> as AuditEntry — ordered)
                 └─ egress_tx.try_send(event)
                       ├─ Full   → egress_dropped += 1
                       └─ Closed → debug!, continue
  → log_audit(AuditEntry { correlation_id: Some(cid), ... })

                  drain task (TaskClass::Telemetry)
                                ↓
                       Agent::record_egress
                 update MetricsSnapshot (watch), push SecurityEvent
                                ↓
                            TUI panel
```

---

## 4. Key Invariants

### Always (without asking)

- Every `EgressEvent` carries a `correlation_id` that matches its parent
  `AuditEntry`'s `correlation_id`. No orphan egress records.
- JSONL sink ordering is preserved: the file-level `tokio::sync::Mutex<File>`
  serializes all writes (egress + audit) on the same destination.
- `try_send` is used for telemetry push — never `.send().await` in the tool hot
  path.
- `egress_dropped_total` is the single source of truth for channel-saturation
  drops and is surfaced in the TUI.
- SSRF / domain / scheme enforcement remains in `validate_url` +
  `check_domain_policy` + `resolve_and_validate`. `EgressEvent` is observational
  only.
- `[tools.egress]` never carries domain allow/deny lists — those live in
  `[tools.scrape]` alone.
- TUI stdout is never corrupted: `AuditLogger::from_config` already redirects
  `stdout → audit.jsonl` in `--tui` mode; egress writes inherit.

### Ask First

- Enlarging the egress channel capacity above 256 — document the justification
  and revisit whether the agent is issuing bursts that deserve backpressure
  instead.
- Adding an `EgressEvent` emitter to a non-executor path (e.g., LLM provider
  HTTP client) — requires a new spec child + threat-model review.

### Never

- Never emit an `EgressEvent` with a missing or empty `correlation_id`.
- Never treat `EgressEvent` as an enforcement point — it is logging, not policy.
- Never add `type: "audit"` to `AuditEntry` for discriminator purposes; use
  presence of `kind` on `EgressEvent` instead.
- Never block the executor tool path on audit/telemetry I/O.
- Never write raw JSON to stdout when `--tui` is active.

---

## 5. Edge Cases and Error Handling

| Case | Behavior |
|---|---|
| Redirect chain with block at hop 2 | Emit `EgressEvent{hop=0, status=Some(302)}`, `EgressEvent{hop=1, blocked=true, reason="ssrf"}`; no hop=2 event. |
| DNS / connect / TLS failure | Emit `EgressEvent{status=None, blocked=false, block_reason=None, duration_ms=<measured>}`. |
| Body exceeds `max_body_bytes` | Emit `EgressEvent{status=Some, response_bytes=<truncated_size>, blocked=false}`. |
| Tool call caller_id missing | `caller_id: None` in both `EgressEvent` and `AuditEntry`; record still emitted. |
| Channel saturation | `egress_dropped_total += 1`, event discarded, executor continues. Drop counter is surfaced. |
| Receiver dropped (shutdown) | `tracing::debug!` once; subsequent sends silently noop. No panic. |
| Non-HTTP tool (shell, memory) | Not instrumented; no `EgressEvent`. Audit entry still carries `correlation_id`. |

---

## 6. Testing Requirements

Mandatory `.local/testing/playbooks/egress-logging.md` scenarios:

1. 3-hop redirect chain with final 200 — expect 3 `EgressEvent`s sharing one
   `correlation_id`, `hop` increments.
2. 3-hop chain with private IP at hop 2 — expect block at hop 1, no hop-2
   event, `SecurityEventCategory::EgressBlocked` pushed.
3. Burst of 300 fetches in a tight loop — `egress_dropped_total > 0`, drop
   counter visible in TUI.
4. `log_hosts_to_tui = false` — JSONL has full host, TUI row shows `"***"`.
5. `EgressEvent` ↔ `AuditEntry` correlation via `correlation_id` on the same
   tool call.
6. `--tui` mode: audit stream redirected to `audit.jsonl`; both `AuditEntry`
   and `EgressEvent` lines interleave correctly.

Unit tests (mandatory):
- `EgressEvent` JSON round-trip — verify `kind: "egress"` and
  `correlation_id` always present; `response_bytes: 0` when disabled.
- `AuditLogger::log_egress` ordering under concurrent writes.
- `try_send` drop-counter accuracy under a synthetic saturation test.

---

## 7. Coverage and Documentation

- Playbook: `.local/testing/playbooks/egress-logging.md` (new, required).
- Coverage row: `.local/testing/coverage-status.md` — `Egress logging | Untested`.
- Wizard prompts: `src/init/security.rs` — prompt for `tools.egress.enabled`,
  `log_blocked`, `log_response_bytes`, `log_hosts_to_tui`. No domain prompts
  under egress.
- Migration: `src/commands/migrate.rs` — insert `[tools.egress]` defaults when
  absent. No domain-list migration.
- CHANGELOG: `[Unreleased]` — feature entry and breaking-note about the new
  `AuditEntry.correlation_id` field (pre-v1 — no deprecation window).

---

## 8. Related Specifications

- `[[010-4-audit]]` — parent audit trail contract (`AuditEntry`, `AuditLogger`).
- `[[010-3-authorization]]` — SSRF / domain policy owns enforcement.
- `[[006-tools/spec]]` — tool executor contract.
- `[[039-background-task-supervisor/spec]]` — `TaskClass::Telemetry` semantics.
- `[[010-6-vigil-intent-anchoring]]` — sibling spec sharing this PR's scope.
