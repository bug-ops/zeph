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
    pub timestamp: String,           // Unix timestamp (seconds) when invocation started
    pub tool: ToolName,              // Tool identifier (e.g., "shell", "web_scrape")
    pub command: String,             // Human-readable command or URL being invoked
    pub result: AuditResult,         // Outcome of the invocation (success/failure)
    pub duration_ms: u64,            // Wall-clock duration in milliseconds
    pub error_category: Option<String>,  // Fine-grained error category label (if failed)
    pub error_domain: Option<String>,    // High-level error domain for recovery
    pub error_phase: Option<String>,     // Invocation phase where error occurred
    pub claim_source: Option<ClaimSource>,  // Provenance of tool result
    pub mcp_server_id: Option<String>,  // MCP server ID (if routed through McpToolExecutor)
    pub injection_flagged: bool,     // Tool output flagged by regex injection detection
    pub embedding_anomalous: bool,   // Tool output flagged by embedding guard anomaly
    pub cross_boundary_mcp_to_acp: bool,  // Tool result crossed MCP-to-ACP trust boundary
    pub adversarial_policy_decision: Option<String>,  // Adversarial policy decision
    pub exit_code: Option<i32>,      // Process exit code for shell executions
    pub truncated: bool,             // Whether output was truncated before storage
    pub caller_id: Option<String>,   // Caller identity that initiated this call
    pub policy_match: Option<String>,    // Policy rule trace that matched
    pub correlation_id: Option<String>,  // Correlation ID shared with associated EgressEvent
    pub vigil_risk: Option<VigilRiskLevel>,  // VIGIL risk level when gate flagged output
}
```

Sanitization before logging:
- Input/output truncated before storage if oversized
- Known secret keys redacted: `api_key`, `token`, `password`, `secret`, `auth`, `key`
- Vault-resolved secrets never logged
- PII scrubbed via `[security.pii_filter]` regex patterns (SSN, credit card, email)

**Configuration** (`[tools.audit]`):
```toml
[tools.audit]
enabled = true                     # Enable audit logging (default: true)
destination = "stdout"             # Log destination: "stdout", "stderr", or file path
# tool_risk_summary = false        # Log per-tool risk summary at startup (default: false)
```

## Authorization & Shell Execution Details

Authorization denials and shell execution details are captured within the unified `AuditEntry` structure:
- **Authorization**: `AuditEntry.policy_match` contains the matched policy rule; `AuditEntry.adversarial_policy_decision` records the decision (`allow`/`deny:<reason>`/`error:<message>`)
- **Shell execution**: `AuditEntry.command` is the sanitized shell command; `AuditEntry.exit_code` is the process exit code; `AuditEntry.duration_ms` is wall-clock execution time

All command text is sanitized before logging: `sudo`, passwords, and secret env vars are redacted from the `command` field.

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

## Turn Boundary Isolation & Signal Accumulation

One signal-processing system operates at the session scope:

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

## Audit Log Storage & Serialization

Immutable persistence via `AuditLogger` (`crates/zeph-tools/src/audit.rs:110`), which serializes entries as flat JSON objects (newline-terminated JSONL format):

```rust
pub struct AuditLogger { /* destination: AuditDestination */ }

impl AuditLogger {
    /// Log a single audit entry asynchronously.
    pub async fn log(&self, entry: &AuditEntry);
}
```

**Destination types** (`AuditDestination`):
- **Stdout** (default): entries written to standard output, one JSON line per entry
- **Stderr**: entries written to standard error
- **File**: entries written to a file path (created with `0o600` permissions on Unix)

All entries are serialized as compact JSON objects (newline-terminated JSONL). Optional fields are omitted to keep entries compact.

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
