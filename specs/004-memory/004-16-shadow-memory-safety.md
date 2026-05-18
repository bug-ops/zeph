---
aliases:
  - MAGE Shadow Memory
  - Trajectory Risk Accumulator
  - Shadow Memory Defense
tags:
  - sdd
  - spec
  - memory
  - security
  - experimental
created: 2026-05-18
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-7-memory-apex-magma]]"
  - "[[001-system-invariants/spec]]"
  - "[[010-security/010-7-shadow-memory-guardrail]]"
---

# Spec: Shadow Memory Safety — Trajectory-Level Attack Defense (MAGE)

> [!info]
> Implements a parallel shadow memory stream that accumulates safety-critical
> signals across an agent's full execution trajectory, enabling detection and
> blocking of multi-turn attacks that evade per-turn controls.
> Resolves GitHub issue [#3695](https://github.com/rabax/zeph/issues/3695).

## Sources

### External
- **MAGE: Multi-turn Agent Guard with Extended memory** (arXiv:2605.03228, 2026) —
  shadow memory for trajectory-level threat detection; reduces tool-attack chaining
  from 100% → 8.3% success rate; eliminates persistent indirect prompt injection

### Internal

| File | Contents |
|------|----------|
| `crates/zeph-sanitizer/src/lib.rs` | `ContentSanitizer`, `PolicyGate`, per-turn audit events |
| `crates/zeph-agent-tools/src/executor.rs` | Tool execution gate; pre-execution policy check |
| `crates/zeph-memory/src/semantic/mod.rs` | `SemanticMemory`; shadow memory is a sibling stream |
| `crates/zeph-core/src/agent/mod.rs` | Agent turn loop; `MemoryState` lifecycle |

---

## 1. Overview

### Problem Statement

Zeph's current safety controls in `zeph-sanitizer` operate per-turn: `ContentSanitizer`
filters individual messages and `PolicyGate` enforces policies on individual tool
invocations. These mechanisms cannot detect cumulative threats that unfold gradually
across multiple turns — a class of attack where no single turn triggers a policy
violation but the trajectory as a whole enacts adversarial behavior.

Three concrete threat classes are undetected today:

1. **Sequential tool-attack chaining**: an adversary plants a goal across 3–5 turns
   (each plausible in isolation), then triggers execution in a later turn. Per-turn
   controls see only individual turns and admit all of them.
2. **Persistent indirect prompt injection**: malicious instructions injected via tool
   output (e.g., a web-scrape result) persist in episodic memory and are recalled in
   future turns, re-injecting the adversarial directive.
3. **Multi-turn poisoning**: repeated low-severity signals accumulate into a high-risk
   trajectory. Each signal alone falls below the per-turn policy threshold.

MAGMA [[004-7-memory-apex-magma]] tracks semantic entity relationships but does not
accumulate trajectory-level safety signals. SafeAgent (#3570) addresses trajectory-
stateful mediation conceptually but remains unimplemented.

### Goal

Implement `TrajectoryRiskAccumulator` — a lightweight shadow memory attached to each
agent session — that ingests per-turn audit events from `zeph-sanitizer`, maintains
a rolling trajectory risk score, and gates tool execution when cumulative risk exceeds
a configured threshold. No changes to the primary memory pipeline are required; shadow
memory is an orthogonal stream.

### Out of Scope

- Replacing or modifying the per-turn `ContentSanitizer` or `PolicyGate` (shadow memory
  is additive, not a substitute)
- Cross-session risk propagation (risk resets when the session ends)
- LLM-based intent classification of each turn (signal detection is rule-based to keep
  overhead low; LLM escalation is a separate optional path)
- Changes to the MAGMA semantic graph schema
- Modifying `zeph-sanitizer` internals beyond adding audit event emission

---

## 2. User Stories

### US-001: Multi-turn attack detection
AS AN operator running long-lived Zeph agents in production
I WANT the agent to detect and block tool-attack chains that develop over multiple turns
SO THAT an adversary who plants context incrementally cannot reach tool execution

**Acceptance criteria:**
```
GIVEN a session where turns 1–4 each emit one low-severity policy warning
  AND turn 5 requests a tool execution
WHEN the cumulative trajectory risk exceeds the configured threshold
THEN the tool execution is denied
AND the agent returns a rationale explaining the denial
AND the incident is logged in the sanitizer audit trail
```

### US-002: Persistent prompt injection blocking
AS AN operator
I WANT recalled tool output that contains injection patterns to be flagged at the
trajectory level
SO THAT injected instructions planted via episodic memory do not execute silently

**Acceptance criteria:**
```
GIVEN a tool result containing a prompt-injection pattern was stored in episodic memory
  AND the injected instruction is recalled in a later turn
WHEN the recall surface triggers a prompt-injection signal
THEN the TrajectoryRiskAccumulator records a high-severity injection signal
AND if the trajectory risk exceeds the threshold, the current tool call is blocked
AND the event is emitted to the audit log
```

### US-003: Benign session pass-through
AS A regular user running normal agent tasks
I WANT safety checks to add no observable latency on clean sessions
SO THAT the security mechanism does not degrade normal operations

**Acceptance criteria:**
```
GIVEN a session with no policy violations, no anomalous tool patterns, and
  no injection signals across 50 turns
WHEN the agent processes each turn
THEN no tool calls are denied by the TrajectoryRiskAccumulator
AND per-turn overhead from shadow memory is < 1 ms at p95
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL maintain one `TrajectoryRiskAccumulator` per agent session, created at session start and dropped at session end | must |
| FR-002 | WHEN `zeph-sanitizer` emits an `AuditEvent` THE SYSTEM SHALL ingest it into the session's `TrajectoryRiskAccumulator` within the same turn | must |
| FR-003 | `TrajectoryRiskAccumulator` SHALL accumulate a `trajectory_risk` score in `[0.0, 1.0]` via a weighted sum of ingested signals with exponential temporal decay per `risk_halflife_turns` | must |
| FR-004 | WHEN `zeph-agent-tools` prepares a tool execution THE SYSTEM SHALL query the session's `TrajectoryRiskAccumulator` for the current `trajectory_risk` | must |
| FR-005 | WHEN `trajectory_risk` ≥ `risk_threshold` (default `0.75`) THE SYSTEM SHALL block the tool execution and return a `ToolError::TrajectoryRiskExceeded { score, signals }` to the agent loop | must |
| FR-006 | WHEN `trajectory_risk` is in `[escalation_threshold, risk_threshold)` (default `[0.5, 0.75)`) THE SYSTEM SHALL escalate to human confirmation before allowing tool execution | should |
| FR-007 | Signal types ingested from `AuditEvent` SHALL include at minimum: `policy_violation`, `prompt_injection_pattern`, `tool_chain_anomaly`, `confidence_drop` | must |
| FR-008 | Each signal type SHALL carry a configurable `base_weight` in `(0.0, 1.0]` and a configurable `severity_multiplier` in `{low=0.5, medium=1.0, high=2.0}` | must |
| FR-009 | `TrajectoryRiskAccumulator` SHALL apply temporal decay: at each new turn, all accumulated signal contributions are multiplied by `exp(-ln(2) / risk_halflife_turns)` before the new turn's signals are added | must |
| FR-010 | WHEN a tool execution is blocked THE SYSTEM SHALL emit an `AuditEvent::TrajectoryBlock { trajectory_risk, top_signals, turn_count }` to the sanitizer audit log | must |
| FR-011 | The TUI SHALL display the current session `trajectory_risk` as a gauge in the security panel when `[memory.shadow_memory] enabled = true` and `tui_show_risk_gauge = true` | should |
| FR-012 | Config flag `[memory.shadow_memory] enabled` SHALL gate all shadow memory code paths; when `false`, `TrajectoryRiskAccumulator` is a no-op struct that always returns `trajectory_risk = 0.0` | must |
| FR-013 | WHEN shadow memory is enabled THE SYSTEM SHALL emit Prometheus counters: `shadow_memory_signals_total{type}`, `shadow_memory_blocks_total`, `shadow_memory_escalations_total` | should |
| FR-014 | Every new code path introduced by this spec SHALL be instrumented with `tracing::info_span!` per the naming convention `memory.shadow.<operation>` | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | `TrajectoryRiskAccumulator::ingest` SHALL complete in < 0.5 ms at p99 (in-memory accumulation only; no I/O on the hot path) |
| NFR-002 | Performance | `TrajectoryRiskAccumulator::current_risk` query (called before each tool execution) SHALL complete in < 0.1 ms at p99 |
| NFR-003 | Performance | When `enabled = false`, shadow memory code contributes zero overhead — all calls are dispatched through a zero-cost no-op implementation |
| NFR-004 | Reliability | `TrajectoryRiskAccumulator` is session-scoped; a session crash or reset creates a fresh accumulator with `trajectory_risk = 0.0`. No persistence is required |
| NFR-005 | Reliability | Shadow memory NEVER blocks the agent loop on I/O. Signal ingestion is synchronous and in-memory only |
| NFR-006 | Security | The shadow memory stream is separate from the primary `SemanticMemory` pipeline; signals from shadow memory are NEVER written to SQLite or Qdrant as user-visible memory |
| NFR-007 | Observability | Prometheus counters export `shadow_memory_signals_total{type,severity}`, `shadow_memory_blocks_total`, `shadow_memory_escalations_total` |
| NFR-008 | Maintainability | Signal type registry is a configurable TOML section; operators can add new signal types and adjust weights without code changes |

---

## 5. Data Model

Shadow memory is entirely in-process and session-scoped. No new database tables are
required.

### `TrajectoryRiskAccumulator` struct

```
TrajectoryRiskAccumulator {
    session_id: SessionId,
    turn_count: u32,
    trajectory_risk: f64,           // current accumulated score ∈ [0.0, 1.0]
    signal_history: Vec<SignalEvent>, // capped ring buffer (last N signals)
    config: ShadowMemoryConfig,
}
```

### `SignalEvent`

```
SignalEvent {
    turn_id: u32,
    signal_type: SignalType,
    severity: Severity,       // Low | Medium | High
    raw_score: f64,           // base_weight × severity_multiplier
    timestamp: Instant,
}
```

### `SignalType` (extensible enum)

| Variant | Source | Default base_weight |
|---------|--------|---------------------|
| `PolicyViolation` | `AuditEvent::PolicyViolation` | 0.30 |
| `PromptInjectionPattern` | `AuditEvent::InjectionDetected` | 0.50 |
| `ToolChainAnomaly` | `AuditEvent::ToolChainPattern` | 0.25 |
| `ConfidenceDrop` | `AuditEvent::ConfidenceDrop` | 0.15 |

### `AuditEvent` additions (in `zeph-sanitizer`)

Two new variants emitted by existing per-turn checks:

```
AuditEvent::ToolChainPattern { turn_id, tool_sequence, anomaly_score }
AuditEvent::TrajectoryBlock { trajectory_risk, top_signals, turn_count }
```

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `trajectory_risk` overflows 1.0 from accumulated signals | Clamp to 1.0; do not error |
| `AuditEvent` ingestion panics (bug in signal parsing) | Catch unwind; log `WARN`; treat as zero-signal; never crash the agent loop |
| Session resets mid-turn (e.g., context compaction) | `TrajectoryRiskAccumulator` is tied to the session; compaction does not reset it unless `reset_on_compaction = true` (config opt-in) |
| Tool execution denied; agent loop retries with a different tool | Each retry re-queries `current_risk`; if risk has not decayed below threshold, retry is also blocked |
| Human escalation response is "deny" | Block recorded as a block event; risk score unchanged (escalation itself does not affect score) |
| `risk_halflife_turns = 0` (misconfiguration) | Treat as `risk_halflife_turns = 1`; log `WARN` at startup |
| Shadow memory disabled at runtime | All paths return no-op immediately; no signals accumulated; no blocks issued |
| Injection pattern detected in recalled (not fresh) content | Signals are emitted by recall surface checks in `zeph-sanitizer`; ingested by accumulator identically to fresh-content signals |

---

## 7. Config

```toml
[memory.shadow_memory]
enabled = false                        # opt-in; default off

risk_threshold = 0.75                  # block tool execution at or above this score
escalation_threshold = 0.50            # escalate to human confirmation above this score
risk_halflife_turns = 10               # decay half-life in agent turns
signal_history_cap = 200              # ring buffer max capacity
tui_show_risk_gauge = true             # show trajectory_risk gauge in TUI security panel
reset_on_compaction = false           # reset accumulator on context compaction

[memory.shadow_memory.signal_weights]
policy_violation     = 0.30
prompt_injection     = 0.50
tool_chain_anomaly   = 0.25
confidence_drop      = 0.15

[memory.shadow_memory.severity_multipliers]
low    = 0.5
medium = 1.0
high   = 2.0
```

---

## 8. Key Invariants

### Always (without asking)
- One `TrajectoryRiskAccumulator` per session; created at session start, dropped at session end
- Signal ingestion is synchronous, in-memory, and completes before the turn continues
- `trajectory_risk` is clamped to `[0.0, 1.0]` at all times
- Shadow memory signals are never written to primary `SemanticMemory` stores (SQLite, Qdrant)
- Temporal decay is applied at the start of each turn before new signals are added
- `enabled = false` is a zero-overhead no-op — no allocations, no checks

### Ask First
- Changing `risk_threshold` below 0.5 (increases false-positive rate significantly)
- Adding new `SignalType` variants (requires validation of base_weight calibration)
- Enabling cross-session risk accumulation (introduces session-state persistence complexity)
- Exposing `trajectory_risk` in user-visible agent output (privacy and gaming concerns)

### Never
- Block the agent turn thread on I/O from within shadow memory
- Write shadow memory signals to `graph_edges`, `messages`, or any primary store
- Return shadow memory state in default recall paths
- Allow `TrajectoryRiskAccumulator` to survive session reset without explicit opt-in

---

## 9. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Sequential tool-attack chaining success rate (lab scenario) | ≤ 10% with default config |
| SC-002 | Persistent indirect prompt injection success rate | 0% — blocked by injection signal weight |
| SC-003 | False-positive block rate on benign 50-turn sessions | < 1% |
| SC-004 | Per-turn shadow memory overhead (ingest + query) | < 1 ms at p95 |
| SC-005 | Prometheus counters exported when enabled | All 3 counter families present |

---

## 10. Acceptance Criteria

```
GIVEN shadow_memory.enabled = true
  AND a session accumulates 5 turns each emitting one PolicyViolation (medium severity)
WHEN the agent attempts a tool call on turn 6
THEN trajectory_risk = f(5 × 0.30 × 1.0 × decay_factor) is computed correctly
AND IF trajectory_risk ≥ 0.75 the tool is blocked with ToolError::TrajectoryRiskExceeded
AND shadow_memory_blocks_total increments
AND AuditEvent::TrajectoryBlock is emitted to the audit log

GIVEN shadow_memory.enabled = false
WHEN the agent processes any number of turns
THEN TrajectoryRiskAccumulator::current_risk always returns 0.0
AND no Prometheus counters are updated
AND no audit events of type TrajectoryBlock are emitted

GIVEN a session with 50 clean turns (no AuditEvents of tracked signal types)
WHEN the agent processes turn 51
THEN trajectory_risk = 0.0
AND no tool call is blocked by shadow memory
```

---

## 11. Implementation Notes

- New module: `crates/zeph-memory/src/shadow/mod.rs` — owns `TrajectoryRiskAccumulator`
  and `SignalEvent`. No dependency on graph or semantic memory modules.
- `zeph-sanitizer` gains two new `AuditEvent` variants (`ToolChainPattern`,
  `TrajectoryBlock`) — additive change, no existing variant modified.
- `zeph-agent-tools` wires the accumulator into the pre-tool-execution gate; receives it
  as an `Arc<Mutex<TrajectoryRiskAccumulator>>` from the session context.
- Signal weight calibration: start with the MAGE paper's reported thresholds; adjust via
  integration tests against known-attack scenarios.
- The in-memory ring buffer for `signal_history` is sized by `signal_history_cap` (default
  200 entries); oldest entries evicted when capacity is reached. The risk score itself is
  not affected by eviction — it is a running accumulator, not recomputed from history.
- Temporal decay formula: at each turn boundary, `trajectory_risk *= exp(-ln(2) / halflife)`.
  This ensures the score halves every `risk_halflife_turns` turns without any signals.
- No database migration is required for this feature.
- TUI gauge integration uses the existing security panel widget added in `zeph-tui`.

---

## 12. Open Questions

> [!question]
> - **Escalation UX**: when `trajectory_risk` is in the escalation band, the agent
>   pauses for human confirmation. The confirmation channel in CLI mode is a blocking
>   prompt; in Telegram/Discord modes it is an async message-reply. The exact API for
>   channel-agnostic human confirmation is not yet defined. This must be resolved before
>   the escalation path (FR-006) can be implemented.
> - **Signal calibration**: the default `base_weight` values are derived from the MAGE
>   paper's scenario descriptions but have not been validated against Zeph's specific
>   attack surface. Calibration experiments should be run before enabling this feature
>   in production configs.

---

## 13. See Also

- [[constitution]] — project principles
- [[004-memory/spec]] — memory system parent index
- [[004-7-memory-apex-magma]] — APEX-MEM (orthogonal: semantic graph, not safety signals)
- [[001-system-invariants/spec]] — system-wide invariants
- [[MOC-specs]] — all specifications
