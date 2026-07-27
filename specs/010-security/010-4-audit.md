---
aliases:
  - Audit Trail
  - Security Logging
  - Tool Execution Audit
  - Authorization Logging
tags:
  - sdd
  - spec
  - security
  - infra
created: 2026-04-10
status: complete
related:
  - "[[010-security/spec]]"
  - "[[010-1-vault]]"
  - "[[010-2-injection-defense]]"
  - "[[010-3-authorization]]"
  - "[[010-5-egress-logging]]"
---

# Spec: Audit Trail & Security Logging

Tool execution auditing, authorization logging, security event correlation, compliance logging.

## Overview

Zeph maintains an immutable audit trail of security-relevant events: tool invocations, authorization decisions, IPI detections, shell command executions. This log is used for compliance, incident investigation, and pattern detection.

## Key Invariants

**Always:**
- All tool invocations logged with: tool name, input (sanitized), output (preview), status, duration
- All authorization denials logged with: agent/skill, tool, reason, policy rule matched
- All shell command execution logged with: command (sanitized), exit code, duration
- All secrets redacted from logs — never log API keys, vault key names, PII
- Audit log persists immutably to disk

**Never:**
- Log secret values, API keys, or PII directly — always sanitize
- Truncate or modify audit entries after creation
- Disable audit logging, even in debug/testing modes

## Tool Execution Audit

Tool executor logs every call to an `AuditEntry`:

```rust
pub struct AuditEntry {
    pub tool: String,                // Tool name
    pub command: String,             // Sanitized command/args
    pub result: String,              // Sanitized output preview (first N chars)
    pub duration_ms: u64,            // Execution time
    pub error_category: Option<String>,  // Error type if failed
    pub caller_id: String,           // Agent/skill that invoked
    pub correlation_id: String,      // Trace ID for related events
    pub timestamp: i64,              // Unix seconds
    pub vigil_risk: Option<f32>,     // Vigil gate risk score (if calculated)
}
```

Sanitization before logging:
- Input/output truncated to max 500 chars
- Known secret keys redacted: `api_key`, `token`, `password`, `secret`, `auth`, `key`
- Vault-resolved secrets never logged
- PII scrubbed via regex (SSN, credit card, email patterns)

## Authorization Audit

`PolicyEnforcer` violations logged immediately:

```rust
pub struct AuthViolationEntry {
    pub skill: String,              // Skill or agent ID
    pub tool: String,               // Tool name
    pub rule_matched: String,       // Policy rule that denied
    pub reason: String,             // Human-readable reason
    pub timestamp: i64,
}
```

## Shell Execution Audit

`ShellExecutor` logs all subprocess invocations:

```rust
pub struct ShellAuditEntry {
    pub command: String,            // Sanitized command
    pub exit_code: i32,             // Process exit code
    pub stderr_preview: String,     // Stderr (first N chars)
    pub duration_ms: u64,
    pub caller_id: String,
    pub timestamp: i64,
}
```

The command is sanitized: `sudo`, passwords, and secret env vars redacted.

## IPI Detection Audit

Injection detection results logged via `AuditSignal`:

```rust
pub enum AuditSignalType {
    PolicyViolation,                // Tool blocked by policy
    PromptInjectionPattern,         // Regex pattern matched
    ToolChainAnomaly,               // Unexpected tool sequence
    ConfidenceDrop,                 // LLM confidence dropped
}

pub enum Severity {
    Low,      // Minor concern
    Medium,   // Warrants tracking
    High,     // Strong indicator
}

pub struct AuditSignal {
    pub signal_type: AuditSignalType,
    pub severity: Severity,
}
```

> [!warning] Architectural gap
> `TrajectoryRiskAccumulator` maintains a **per-session, cross-turn risk score with exponential decay** and makes **hard-blocking tool-execution decisions** when risk exceeds a threshold. This appears to violate or at least is not addressed by the parent spec's NEVER rule, which forbids cross-turn accumulation "for injection-confirmation decisions." The spec carves out an exception for `TrajectorySentinel` (advisory-only, reversible decay), but does not discuss `TrajectoryRiskAccumulator` at all. Whether this system should be governed by the NEVER rule or falls outside its scope (because its decisions are general safety-gating, not injection-confirmation) is an open architectural question that needs resolution.

## Turn Boundary Isolation

Two distinct signal-processing systems operate at different scopes:

**`CrossToolCorrelator`** — detects injection patterns **within a single turn**:

```rust
pub struct CrossToolCorrelator {
    // Stateful: tracks per-tool injection detections within current turn
}

impl CrossToolCorrelator {
    pub fn ingest_detection(&mut self, signal: &AuditSignal) -> Option<InjectionConfirmed>;
    
    pub fn clear_on_turn_boundary(&mut self);  // Called at every turn start
}
```

**Key constraint** (from parent spec): state is cleared at turn boundaries. Multiple injection signals within turn N can trigger `InjectionConfirmed`, but signals never carry into turn N+1. This prevents a single noisy turn from poisoning all subsequent turns.

**`TrajectoryRiskAccumulator`** — accumulates signals **per-session, across turn boundaries** with exponential decay:

```rust
pub struct TrajectoryRiskAccumulator {
    // Maintains trajectory_risk: [0.0, 1.0]
    // Decays exponentially between turns
    // Makes hard-blocking decisions when risk >= threshold
}

impl TrajectoryRiskAccumulator {
    pub fn is_blocked(&self) -> bool;  // Hard block: risk >= threshold
    pub fn ingest(&mut self, signal: &AuditSignal);  // Cross-turn accumulation
    pub fn advance_turn(&mut self);  // Apply exponential decay at turn boundary
}
```

**Status**: Not explicitly addressed in parent spec's NEVER rule. Whether this system's cross-turn, hard-blocking behavior is intended carve-out (like `TrajectorySentinel`) or an overlooked violation remains unresolved.

## Audit Log Storage

Immutable persistence via `zeph-db`:

```rust
pub struct AuditLogger { /* ... */ }

impl AuditLogger {
    /// Append an entry to the immutable audit log.
    pub async fn log(&self, entry: AuditEntry) -> Result<()>;
    
    /// Query by correlation ID across related events.
    pub async fn query_by_correlation(&self, correlation_id: &str) 
        -> Result<Vec<AuditEntry>>;
}
```

Backend options:
- **SQLite** (default): `audit.db` with append-only table
- **JSONL**: `audit.jsonl` (one entry per line, never edited)

File created with `0o600` permissions (owner-read/write only on Unix).

## Configuration

```toml
[security.audit]
enabled = true
backend = "sqlite"              # "sqlite" or "jsonl"
path = ".local/audit.db"
retention_days = 90             # Optional: archival threshold
log_level = "info"              # "debug", "info", "warn"
```

## Vault Access Logging

Vault reads/writes are NOT logged to audit trail (to prevent metadata leakage), but are tracked via in-memory metrics. Failed vault lookups are logged with redacted key names.

## Integration Points

- [[006-tools/spec]] — Tool execution audited
- [[010-1-vault]] — Credential access tracked (metadata-safe)
- [[010-2-injection-defense]] — IPI `AuditSignal` ingested by trajectory accumulator
- [[010-3-authorization]] — Authorization violations logged
- [[010-5-egress-logging]] — HTTP/webhook egress events logged

## See Also

- [[010-security/spec]] — Parent; cross-turn NEVER rule for hard decisions
- [[010-5-egress-logging]] — HTTP egress audit (`EgressEvent`)
