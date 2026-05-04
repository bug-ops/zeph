---
aliases:
  - Security Capability Governance
  - Capability Scoping
  - Trajectory Sentinel
  - CapSeal Vault Broker
tags:
  - sdd
  - spec
  - security
  - capability
  - governance
  - contract
created: 2026-05-04
revised: 2026-05-04
status: draft
related:
  - "[[001-system-invariants/spec]]"
  - "[[006-tools/spec]]"
  - "[[010-security/spec]]"
  - "[[010-1-vault]]"
  - "[[010-3-authorization]]"
  - "[[010-6-vigil-intent-anchoring]]"
---

# Spec 050: Security Capability Governance

> [!info]
> Three complementary patterns that constrain *what tools the agent can reach*,
> *how it accumulates advisory risk over a multi-step trajectory*, and *how
> secret material is consumed without ever materialising as plaintext to the LLM*.

## Revision Note (2026-05-04)

This spec was revised twice on 2026-05-04. The first revision resolved
critic findings C1–C7; the second applied minor fixes R1–R4:

- **R1**: FR-CG-010 auto-recover hard-resets the score to 0.0 at the
  turn-count cap (no per-signal weight gate). Residual aggregate-attack
  window documented in `## Known Gaps`.
- **R2**: Per-namespace pattern strictness — `builtin:` / `skill:` strict,
  `mcp:` / `acp:` / `a2a:` provisional. New `pattern_strictness` config
  key (`provisional_for_dynamic_namespaces` default).
- **R3**: `alert_threshold` decoupled from `elevated_at` to `high_at`
  (4.0). New NEVER clause forbidding any LLM-reachable surface from
  observing `RiskAlert` / `RiskLevel` / sentinel score.
- **R4**: `subagent_inheritance_factor = 0.5` documented as ≈ one
  half-life of decay (`0.85^4.27`); config validator warns when
  `decay_per_turn` is tightened without adjusting the factor.

The original revision (resolving C1–C7) is summarised below:

- **C1 (invariant conflict).** [[010-security/spec]] now carries an explicit
  carve-out scoping the "no cross-turn signal accumulation" prohibition to
  `CrossToolCorrelator` (an injection-confirmation decision). Advisory risk
  budgeting via `TrajectorySentinel` is permitted under the conditions stated
  in 010-security and re-stated here as Invariant 1.
- **C2 (glob bypass).** Scope patterns are resolved against the **registry-known
  tool-id set at agent build time**; an empty match set is a fatal startup
  error; MCP tools are namespace-prefixed (`mcp:<server>/<tool>`) so a
  capability-id glob cannot accidentally widen across namespaces.
- **C3 (Critical deadlock).** `advance_turn` runs unconditionally **before**
  gate evaluation; an `auto_recover_after_turns` (default 16) hard cap exits
  Critical without operator action; unattended channels (scheduler, Telegram)
  carry an explicit availability invariant.
- **C4 (CapSeal closed-set leak).** Phase 3 sketch is reframed around a
  typestate `BoundSecret<Op>` instead of a closed `SealedIntent` enum; the
  closed-enum design is documented as an explicit anti-pattern.
- **C5 (unjustified weights).** Concrete numeric thresholds for
  `elevated_at`, `high_at`, `critical_at`, and `alert_threshold` are now
  specified, with a derivation. `NovelTool` is dropped from Phase 1 (deferred
  behind a feature flag).
- **C6 (scope-swap audit gap).** The audit entry now carries both
  `scope_at_definition` and `scope_at_dispatch`.
- **C7 (missing signals).** Adds `HighCallRate`, `UnusualReadVolume`, and a
  `ToolPairTransition` signal; subagents inherit the parent's score with a
  damping factor when the parent is at `>= Elevated`.

## Issues Addressed

| Issue | Title | Phase |
|---|---|---|
| #3563 | Aethelgard — RL-learned dynamic capability governance | Phase 1 (static), Phase 2 (RL) |
| #3569 | CapSeal + SUDP — capability-sealed vault-broker | Phase 3 (research/spec only) |
| #3570 | SafeAgent — trajectory-aware risk governance | Phase 1 (heuristic) |

## Motivation

Today's Zeph security stack defends each *individual* tool call: `PolicyEnforcer`
(deny/allow on tool+path+args), `VigilGate` (regex tripwire on tool *output*),
`ContentSanitizer` (spotlighting / quarantine), `PiiFilter`, `ExfiltrationGuard`.
Three orthogonal gaps remain:

1. **Capability over-provisioning.** Every turn the LLM sees the union of all
   `tool_definitions()` from every executor. Measured baseline: ~15× the median
   tools-per-task working set. A wider attack surface inflates injection
   leverage and prompt-token spend.
2. **Trajectory blindness.** `PolicyEnforcer` is stateless across turns. A
   patient adversary can spread an attack across N tool calls — each individual
   call passes policy, but the *sequence* is the attack. There is no advisory
   risk accumulator.
3. **Secrets-as-strings risk.** `VaultProvider::get_secret` returns plaintext
   `String`. Once resolved into a tool arg or HTTP header, the secret is one
   prompt-injection or log-leak away from exfiltration.

Spec 050 closes (1) and (2) in Phase 1 and reserves (3) as a research item
(Phase 3) so that follow-on work has a stable contract to target.

## Requirements

### Functional

| ID | Requirement |
|----|-------------|
| FR-CG-001 | The agent MUST be able to expose a *narrowed* set of `tool_definitions` to the LLM, computed from a per-task-type allow-list, without modifying executor implementations. |
| FR-CG-002 | The narrowed scope MUST be configured declaratively in `[security.capability_scopes.<task_type>]` with a list of glob patterns over **fully-qualified tool ids** (`<namespace>:<tool>`), plus a `default_scope` fallback. |
| FR-CG-003 | When no `task_type` is selected, scoping MUST be the no-op identity (full tool set surfaced — preserves existing behaviour). |
| FR-CG-004 | Tool calls outside the active scope MUST be rejected at `execute_tool_call` with a structured error (`ToolError::OutOfScope`) BEFORE any tool side-effect runs. The audit log MUST record the rejection with `error_category = "out_of_scope"`. |
| FR-CG-005 | At agent build time, every glob in every configured scope MUST be matched against the *materialised* tool registry; a glob that matches **zero** registered tools, or `**` / `*` / `mcp:*` patterns that match the entire registry without an explicit operator opt-in, MUST be a fatal startup error (NFR-CG-004). |
| FR-CG-006 | A `TrajectorySentinel` MUST accumulate risk signals across the last `window_turns` (default 8) of the current session and expose `current_risk() -> RiskLevel` as **advisory input** to the policy gate. |
| FR-CG-007 | When `current_risk() >= alert_threshold`, the sentinel MUST emit a `RiskAlert` consumed by `PolicyGateExecutor` to escalate the next decision. |
| FR-CG-008 | Sentinel signals MUST be source-typed and aggregated with bounded weights so a single noisy signal cannot saturate the score. |
| FR-CG-009 | Sentinel state MUST be cleared on `/clear`, on session-restart, and when the operator runs `/trajectory reset`. |
| FR-CG-010 | After `auto_recover_after_turns` consecutive turns at `Critical` with no new high-weight signal, the sentinel MUST self-decay below `critical_at` so unattended agents (scheduler, Telegram) recover without operator action. |
| FR-CG-011 | When a subagent is spawned while the parent's `current_risk() >= Elevated`, the subagent's `TrajectorySentinel` MUST be initialised with a damped copy of the parent's score (factor `0.5`) so the spawn cannot be used as a free risk reset. |
| FR-CG-012 | The audit entry MUST record both `scope_at_definition` (scope name in effect when `tool_definitions` was assembled for the LLM) and `scope_at_dispatch` (scope name when the call was admitted/rejected). |
| FR-CG-013 | The `propose_operation` vault-broker contract (Phase 3) MUST be specified as a **typestate `BoundSecret<Op>`** in this document; no implementation code is required in Phase 1. |

### Non-Functional

| ID | Requirement |
|----|-------------|
| NFR-CG-001 | Capability scoping overhead MUST be ≤ 50 µs per turn for ≤ 200 tools. |
| NFR-CG-002 | TrajectorySentinel `record_signal` + `current_risk` MUST be O(1) amortised. |
| NFR-CG-003 | All sentinel state MUST live in `SecurityState` (per-agent, not global). Subagent inheritance is via FR-CG-011 only. |
| NFR-CG-004 | Misconfiguration of `[security.capability_scopes]` MUST be a fatal startup error. Patterns matching zero tools are a misconfiguration. |
| NFR-CG-005 | Audit entries for out-of-scope rejections, risk alerts, and Critical downgrades MUST set `error_category` to a stable string for downstream metrics. |
| NFR-CG-006 | Unattended agents (scheduler-spawned, Telegram, A2A server) MUST never block indefinitely on a Critical-deny loop. FR-CG-010 (auto-recover) is the binding mitigation. |

## Component Designs

### 1. `ScopedToolExecutor` (FR-CG-001 .. 005, FR-CG-012)

Lives in `crates/zeph-tools/src/scope.rs`. Reuses the existing `ToolFilter`
pattern in `tool_filter.rs` rather than mutating the `ToolExecutor` trait.

**Why a wrapper, not a trait method.** Issue #3563 proposes
`fn scope_for_task(task: &str) -> Vec<ToolId>` on the trait. That couples
*every* executor to scoping, requires an N-way default, and complicates the
`CompositeExecutor` aggregation. A wrapper inverts the dependency.

**Tool-id namespacing (C2 mitigation).** All tool ids surfaced by Zeph are
prefixed by namespace before scope resolution:

| Source | Namespace |
|---|---|
| Built-in executors (shell, file, web_scrape, …) | `builtin:` |
| Skill-defined tools | `skill:<skill_name>/` |
| MCP tools | `mcp:<server_id>/` |
| ACP / A2A proxied tools | `acp:<peer>/` and `a2a:<peer>/` |

Glob patterns operate on these qualified ids. `mcp:*` is a single-namespace
glob; `*` covers all namespaces and is allowed only in the explicit
`default_scope = "general"` configuration. Any *un-namespaced* tool id
returned by an executor at registration is a fatal startup error — there is
no path for an MCP server to register `search_arbitrary_shell` and have it
match `search_*` configured for builtins.

**Build-time pattern resolution (C2 mitigation).** At agent build, every
configured glob is compiled and matched against the materialised tool
registry once. Resolution strictness is per-namespace:

| Namespace class | Default strictness |
|---|---|
| `builtin:`, `skill:` | strict — registry is fully known at build time |
| `mcp:`, `acp:`, `a2a:` | provisional — registry may grow as servers connect |

- A `builtin:` or `skill:` glob that matches **zero** ids is a fatal
  `ScopeError::DeadPattern`.
- An `mcp:` / `acp:` / `a2a:` glob that matches zero ids at build time is
  recorded as `ScopeWarning::ProvisionalDeadPattern` and re-resolved on
  every dynamic registration. Failing strict on a not-yet-connected MCP
  server would conflict with NFR-CG-006 (unattended channels must boot
  even when the upstream is down).
- A glob that expands to the **entire registry** without `default_scope =
  "general"` opt-in is a fatal `ScopeError::AccidentallyFull` regardless
  of namespace.
- The compiled scope stores the *expanded set of tool ids*, not the raw
  glob — runtime admission becomes a `HashSet<ToolId>` lookup, not glob
  evaluation. The set is rebuilt on every dynamic-namespace registration.
- The `pattern_strictness` config key (`strict` | `permissive` |
  `provisional_for_dynamic_namespaces` — default the third) overrides the
  per-namespace defaults if an operator wants uniform strictness.
- New tools registered after build are re-resolved against the active
  scope on registration; if a new id widens the active scope, the operator
  is warned in the TUI status line.

```rust
// crates/zeph-tools/src/scope.rs
use std::collections::HashSet;
use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;

pub struct ToolScope {
    pub task_type: Option<String>,
    /// Expanded, materialised set of fully-qualified tool ids.
    admitted: HashSet<String>,
    /// `true` for the `general` default-scope only; admits everything.
    is_full: bool,
}

impl ToolScope {
    pub fn full() -> Self { Self { task_type: None, admitted: HashSet::new(), is_full: true } }

    /// Compile globs against the materialised registry. Fatal on dead patterns,
    /// accidental-full, or empty result without an explicit full opt-in.
    pub fn try_compile(
        task_type: impl Into<String>,
        patterns: &[String],
        registry_ids: &HashSet<String>,
        is_default_general: bool,
    ) -> Result<Self, ScopeError> { /* ... */ }

    fn admits(&self, qualified_tool_id: &str) -> bool {
        self.is_full || self.admitted.contains(qualified_tool_id)
    }
}

pub struct ScopedToolExecutor<E: ToolExecutor> {
    inner: E,
    scope: arc_swap::ArcSwap<ToolScope>,
}

impl<E: ToolExecutor> ToolExecutor for ScopedToolExecutor<E> {
    fn tool_definitions(&self) -> Vec<ToolDef> {
        let scope = self.scope.load();
        self.inner
            .tool_definitions()
            .into_iter()
            .filter(|d| scope.admits(d.qualified_id()))
            .collect()
    }

    async fn execute_tool_call(&self, call: &ToolCall)
        -> Result<Option<ToolOutput>, ToolError>
    {
        let scope = self.scope.load();
        if !scope.admits(call.qualified_tool_id()) {
            return Err(ToolError::OutOfScope {
                tool_id: call.qualified_tool_id().to_string(),
                task_type: scope.task_type.clone(),
            });
        }
        self.inner.execute_tool_call(call).await
    }
}
```

**Wiring order** (outermost first):

```text
ScopedToolExecutor
    → PolicyGateExecutor
        → TrustGateExecutor
            → CompositeExecutor
                → ToolFilter, AuditedExecutor, ...
```

`ScopedToolExecutor` wraps *outside* `PolicyGateExecutor` so an out-of-scope
call short-circuits before policy evaluation.

**Scope-swap semantics (C6 mitigation).** Scope swaps via `/scope <name>`
take effect at the next `execute_tool_call` boundary; in-flight calls
complete under the scope active at admission. The audit emission carries
both `scope_at_definition` (recorded when the tool list was last surfaced
to the LLM) and `scope_at_dispatch`. A divergence between the two is a
forensic signal, not an error — the LLM may legitimately have learned of a
tool from a wider prior scope.

**Config schema:**

```toml
[security.capability_scopes]
default_scope = "general"
strict        = true            # unknown task_type → fatal startup
                                # (false → falls back to default_scope)

[security.capability_scopes.general]
patterns = ["*"]                # explicit identity scope opt-in

[security.capability_scopes.research]
patterns = ["builtin:fetch", "builtin:web_scrape", "builtin:search_*",
            "builtin:read", "builtin:glob", "mcp:*/search_*"]

[security.capability_scopes.code_edit]
patterns = ["builtin:read", "builtin:edit", "builtin:write",
            "builtin:shell", "builtin:glob"]

[security.capability_scopes.shell_only]
patterns = ["builtin:shell"]
```

### 2. `TrajectorySentinel` (FR-CG-006 .. 011)

Lives in `crates/zeph-core/src/agent/trajectory.rs`. Stored as a field on
`SecurityState`.

**Relationship to 010-security.** This subsystem is governed by the explicit
carve-out in [[010-security/spec#Scope of the Cross-Turn Prohibition
(architectural decision, 2026-05-04)]]: the cross-turn prohibition applies
to `CrossToolCorrelator` (irreversible injection-confirmation) only, not to
this advisory, decaying, allow-modulating layer.

**Signal taxonomy (Phase 1):**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskSignal {
    /// VIGIL flagged a tool output.
    VigilFlagged(VigilRiskLevel),
    /// PolicyEnforcer denied a structured tool call.
    PolicyDeny,
    /// `ExfiltrationGuard` redacted at least one outbound URL or HTML img.
    ExfiltrationRedaction,
    /// Tool call rejected as out-of-scope (FR-CG-004).
    OutOfScope,
    /// PII filter redacted ≥ 1 span in a tool output.
    PiiRedaction,
    /// Tool returned a non-zero exit code or unrecoverable error.
    ToolFailure,
    /// More than `high_call_rate_threshold` (default 12) tool calls in
    /// the last 3 turns. (C7-a volumetric)
    HighCallRate,
    /// More than `unusual_read_threshold` (default 24) distinct paths read
    /// in `window_turns`. (C7-c volumetric)
    UnusualReadVolume,
    /// A configured high-risk tool-pair transition occurred within K turns.
    /// E.g. `web_scrape` → `write` to a system path. (C7-d transition)
    ToolPairTransition,
}
```

`NovelTool` (Phase-1 candidate in v1 of this spec) is **dropped from Phase
1** because of conflict with legitimate first-time MCP server use. It is
reserved behind a feature flag (`security.trajectory.novelty_signal`) for
Phase 2 when persistent per-installation novelty tracking is available.

**Weights and thresholds (C5 mitigation, all configurable):**

| Symbol | Default | Justification |
|---|---:|---|
| `weight(VigilFlagged(High))` | 2.5 | A confirmed high-risk injection candidate; two of them in a short window is by itself worrying. |
| `weight(VigilFlagged(Medium))` | 1.0 | Medium-confidence injection match. |
| `weight(VigilFlagged(Low))` | 0.3 | (Reserved; current `VigilGate` does not emit Low.) |
| `weight(PolicyDeny)` | 1.5 | Denied calls are themselves policy-conformant — but a *cluster* indicates the LLM is testing the perimeter. |
| `weight(ExfiltrationRedaction)` | 2.0 | Active redaction is closer to evidence than VigilFlagged. |
| `weight(OutOfScope)` | 1.5 | Same family as PolicyDeny. |
| `weight(PiiRedaction)` | 0.5 | Common in legitimate data work; small weight. |
| `weight(ToolFailure)` | 0.3 | Noisy signal; tiny weight to avoid alert fatigue. |
| `weight(HighCallRate)` | 1.5 | Fires at most once per 3-turn window. |
| `weight(UnusualReadVolume)` | 1.5 | Fires at most once per `window_turns`. |
| `weight(ToolPairTransition)` | 2.0 | Fires only on configured anti-patterns; cardinality is operator-bounded. |
| `decay_per_turn` | 0.85 | Half-life ≈ 4.3 turns; balances slow-burn detection vs. recovery. |
| `window_turns` | 8 | Twice the half-life; bounds memory. |
| `elevated_at` | 2.0 | One PolicyDeny + one PiiRedaction crosses it. |
| `high_at` | 4.0 | Two simultaneous high-weight signals or a sustained moderate cluster. |
| `critical_at` | 8.0 | Three concurrent high-weight events; calibrated against the saturation ceiling. |
| `alert_threshold` | `high_at` (4.0) | Decoupled from `elevated_at`: routine activity (e.g. one `PolicyDeny` + one `PiiRedaction` ≈ 2.0) crosses Elevated and would flood the alert stream. Alerts fire only at `High` or above so they remain operator-actionable. |
| `auto_recover_after_turns` | 16 | Hard cap on Critical persistence (FR-CG-010). |
| `subagent_inheritance_factor` | 0.5 | Spawn-time damping of inherited score (FR-CG-011). Calibrated as ≈ `decay_per_turn ^ 4.27` — one half-life of decay (`0.85^4.27 ≈ 0.5`). Operators tightening `decay_per_turn` MUST adjust this factor; the config validator emits a warning when the relation `subagent_inheritance_factor ≈ decay_per_turn ^ (ln(0.5)/ln(decay_per_turn))` deviates by more than 0.1 in either direction. |

**Saturation ceiling and acceptance test.** All Phase-1 signals at full
weight, sustained at one event per turn for 8 turns, produce a score
asymptotically approaching:

```
score_max = Σ_(k=0..7) 0.85^k × max_weight_per_turn
          ≈ 5.7 × max_weight_per_turn
```

For `max_weight_per_turn = 2.5` (one `VigilFlagged(High)` per turn) the
ceiling is ≈ 14.3, comfortably above `critical_at = 8.0`. The acceptance
test "6 × `VigilFlagged(High)` over 8 turns → Critical" computes:

```
score = Σ_(k=0..5) 0.85^k × 2.5  ≈  10.3   →  Critical (≥ 8.0)
```

A property test asserts that no random Phase-1 signal trace can produce a
NaN or negative score, and that any trace of length ≤ 8 with at most one
event per turn is bounded above by `score_max`.

```rust
pub struct TrajectorySentinel {
    cfg: TrajectorySentinelConfig,
    buf: VecDeque<(u64, RiskSignal)>,   // (turn, signal); evicted by window
    current_turn: u64,
    last_change_turn: u64,              // for auto-recover gating
    cached_score: Option<f32>,          // invalidated on record / advance
}

impl TrajectorySentinel {
    pub fn record(&mut self, sig: RiskSignal) { /* push, evict, mark dirty */ }

    /// MUST be called once per turn, BEFORE the gate evaluates.
    /// Applies multiplicative decay and the FR-CG-010 auto-recover cap.
    pub fn advance_turn(&mut self) { /* see invariants below */ }

    pub fn current_risk(&self) -> RiskLevel { /* score → bucket */ }
    pub fn poll_alert(&self) -> Option<RiskAlert> { /* Some when ≥ Elevated */ }
    pub fn reset(&mut self) { /* full clear */ }

    /// Initialise a child sentinel at subagent spawn time per FR-CG-011.
    pub fn spawn_child(&self) -> TrajectorySentinel {
        let mut child = TrajectorySentinel::new(self.cfg.clone());
        if self.current_risk() >= RiskLevel::Elevated {
            let damped_score = self.score_now() * self.cfg.subagent_inheritance_factor;
            child.seed_score(damped_score);
        }
        child
    }
}
```

**advance_turn ordering invariant (C3 mitigation).** The agent loop calls
`sentinel.advance_turn()` at the **start** of every turn, *before* any
`PolicyGateExecutor::check_policy` runs. This guarantees that even on a
Critical-deny turn:

1. `advance_turn` runs first; multiplicative decay is applied.
2. Gate evaluates with the post-decay score.
3. If still `>= Critical`, the deny stands; otherwise it does not.

Without this ordering, a Critical-deny turn would abort before decay,
producing a monotone deadlock. The agent loop wiring puts `advance_turn`
in the same hot section as `current_turn += 1`, with a tracing span and
`debug_assert!(self.current_turn > prev)`.

**FR-CG-010 auto-recover.** When the sentinel has been at `>= Critical`
for `auto_recover_after_turns` consecutive `advance_turn` calls, the score
is **hard-reset to `0.0`** at the cap and the buffered signal history is
cleared. The reset is unconditional with respect to per-individual-signal
weights — a per-signal gate (e.g. "no signal `>= 1.5`") would be defeated
by an attacker sustaining `VigilFlagged(Medium) + PiiRedaction +
ToolFailure = 1.8/turn` (no individual `>= 1.5`) producing a permanent
16-denied/1-allowed cycle. The hard reset is the only design that breaks
out of every aggregate-attack profile. The reset is logged as
`AuditEntry { error_category: "trajectory_auto_recover", … }` carrying
the score and signal census at reset time for forensics. The threshold can
be raised by operator config but not below a 4-turn floor. This is the
binding mitigation for NFR-CG-006 (unattended channels). See `## Known
Gaps` for the residual aggregate-attack window between resets.

**Consumption.** `PolicyGateExecutor` checks `sentinel.current_risk()`
**after** its own rule evaluation:

| Risk level | Effect on a rule's `Allow` |
|---|---|
| `Calm` / `Elevated` | unchanged; audit entry tagged with `risk_level`. |
| `High` | unchanged in Phase 1; audit entry carries `error_category = "trajectory_alert"`. Phase 2 may require confirmation. |
| `Critical` | downgraded to `Deny { trace: "trajectory_critical" }`. Audit entry carries `error_category = "trajectory_critical"`. |

`Deny` decisions are **never upgraded** by the sentinel.

**Where signals are recorded:**

| Source file | Signal |
|-------------|--------|
| `PolicyGateExecutor::check_policy` | `PolicyDeny` |
| `ScopedToolExecutor::execute_tool_call` | `OutOfScope` |
| `agent/tool_execution/sanitize.rs` | `VigilFlagged`, `PiiRedaction`, `ExfiltrationRedaction` |
| `agent/tool_execution/dispatch.rs` | `ToolFailure`, `HighCallRate`, `UnusualReadVolume`, `ToolPairTransition` |
| `agent/builder.rs` (subagent spawn) | `spawn_child` — see FR-CG-011 |

### 3. CapSeal + SUDP — Phase 3 typestate sketch (FR-CG-013, C4 mitigation)

A Phase 3 sketch only. The earlier draft used a closed `SealedIntent` enum;
the critic correctly flagged that any closed enum becomes the leak surface.
The revised approach is a typestate-bound credential handle:

```rust
// proposed: crates/zeph-vault/src/broker.rs (Phase 3, NOT in this PR)

/// Marker trait for vault operations. Implementors are types, not values.
pub trait VaultOp: sealed::Sealed {
    type Input;
    type Output;
}

/// A bound credential handle. The `Op` parameter selects the only operation
/// that may be performed. Cannot be cloned, cannot be compared, cannot be
/// printed. Drops zero the underlying resolver token.
pub struct BoundSecret<Op: VaultOp> {
    /* opaque */
    _op: PhantomData<Op>,
}

pub mod ops {
    pub struct HttpBearer;       impl VaultOp for HttpBearer { /* ... */ }
    pub struct HmacSign;          impl VaultOp for HmacSign { /* ... */ }
    pub struct Decrypt;           impl VaultOp for Decrypt { /* ... */ }
    // New operations are new types. Adding one requires every dispatcher
    // that handles `BoundSecret<NewOp>` to compile-error if it doesn't
    // explicitly implement the new operation. There is no `match _` arm.
}

pub trait VaultBroker: Send + Sync {
    /// Each operation is a separate trait method. There is no enum, no
    /// `propose_operation(intent)` indirection that hides the operation
    /// type. Adding a new op requires extending `VaultBroker` (or a sister
    /// trait) and is a compile-checked decision at every call site.
    fn http_bearer(
        &self,
        secret: BoundSecret<ops::HttpBearer>,
        request: http::request::Builder,
    ) -> Pin<Box<dyn Future<Output = Result<http::Request<Bytes>, BrokerError>> + Send + '_>>;

    fn hmac_sign(
        &self,
        secret: BoundSecret<ops::HmacSign>,
        payload: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BrokerError>> + Send + '_>>;

    fn decrypt(
        &self,
        secret: BoundSecret<ops::Decrypt>,
        ciphertext: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<Bytes, BrokerError>> + Send + '_>>;
}
```

**Why this beats the closed enum.**
- A new op is a new type, not a new variant. Existing call sites do not
  silently fall through; they fail at compile time if they need the new op.
- `BoundSecret<HttpBearer>` cannot be coerced to `BoundSecret<HmacSign>`;
  a stolen handle cannot be retargeted to a different operation.
- The "LLM cannot invent a new op" property is enforced statically.

**Why this is research-only in Phase 1.** The full design needs an audit
of every `VaultProvider::get_secret` caller to enumerate the actual
operations. That work is Phase 3 scope and is tracked under [[010-1-vault]]
follow-on.

## Key Invariants

1. **Sentinel is advisory and decaying.** Per the explicit carve-out in
   [[010-security/spec]], `TrajectorySentinel` is permitted to accumulate
   across turns because (a) its outputs are advisory modulators of existing
   gate decisions, never new verdicts; (b) signals decay multiplicatively
   each turn so noise cannot durably elevate; (c) sentinel can downgrade
   `Allow` to `Deny` but never the reverse.
2. **`advance_turn` precedes gate evaluation.** Decay is applied at the
   start of every turn before any `PolicyGateExecutor::check_policy` runs.
3. **Critical state is bounded.** `auto_recover_after_turns` (default 16)
   is a hard cap on Critical persistence in the absence of new high-weight
   signals. Unattended channels rely on this.
4. **Scope is registry-resolved at build time.** Glob patterns are matched
   against the materialised tool registry; dead patterns and accidentally
   full patterns are fatal startup errors.
5. **Tool ids are namespaced before scoping.** `builtin:`, `skill:<name>/`,
   `mcp:<server>/`, `acp:<peer>/`, `a2a:<peer>/`. No cross-namespace
   accidental match.
6. **Sentinel state is per-`SecurityState`** (per-agent). Subagents inherit
   only via `spawn_child` with `subagent_inheritance_factor` damping, and
   only when the parent is `>= Elevated`.
7. **Audit-first.** Every scope rejection, every Critical downgrade, and
   every auto-recover writes an `AuditEntry` *before* returning the error.
8. **Phase 1 is heuristic, not ML.** Weights and thresholds are config
   constants. Phase 2 may add a learning adaptor consuming the same signal
   stream.
9. **CapSeal is opt-in and additive.** Phase 3 adds `VaultBroker` *next to*
   `VaultProvider`, never replacing it implicitly.

## NEVER

- NEVER widen scope at runtime in response to LLM output. `/scope` is an
  operator command; the LLM cannot reach it.
- NEVER attempt scope enforcement on the legacy `execute()` /
  `execute_confirmed()` fenced-block path. Documented carve-out (mirrors
  `PolicyGate` CRIT-03).
- NEVER use `glob::Pattern::matches` against an unqualified, non-namespaced
  tool id. All tool ids are `<namespace>:<id>` before scope resolution.
- NEVER let an MCP server register a tool id that contains a `:` other
  than the namespace separator inserted by the registry.
- NEVER let `tool_definitions()` for one scope be reused for dispatch
  under a different scope without re-checking admission.
- NEVER let the sentinel score go negative or NaN. All weights are
  finite, non-negative; `decay_per_turn ∈ (0, 1]`.
- NEVER block on the sentinel — record/score is in-process and lock-free
  beyond a single `parking_lot::Mutex` on the inner buffer. No async, no
  I/O.
- NEVER apply a sentinel reset implicitly when the LLM asks. Reset is
  operator-only; FR-CG-010 auto-recover is *not* a reset, only a partial
  decay.
- NEVER design a Phase-3 secret broker around a closed enum (e.g.
  `SealedIntent`). Use typestate `BoundSecret<Op>`.
- NEVER let a subagent spawn act as a free risk reset. FR-CG-011
  inheritance is mandatory.
- NEVER expose `RiskAlert`, `RiskLevel`, the sentinel score, or any of its
  internal state to LLM-callable tools, slash commands the LLM can invoke,
  or any context surface the LLM can read. The signal taxonomy and current
  level are operator/forensic data; surfacing them lets a prompt-injected
  LLM probe the gate (e.g. by emitting calibrated noise to characterise
  thresholds, or by detecting auto-recover and timing attacks around it).
  `/trajectory show` is operator-only and goes through the channel control
  path, not the tool surface.

## Implementation Phases

### Phase 1 — this PR (issues #3563 + #3570)

1. `crates/zeph-tools/src/scope.rs` — `ToolScope`, `ScopedToolExecutor`,
   `ScopeError { DeadPattern, AccidentallyFull, … }`. Wired in
   `agent::builder` between `PolicyGateExecutor` and the agent stack.
2. Tool-id namespacing: registry guarantees every `ToolDef.id` carries a
   namespace prefix; un-namespaced ids are rejected at registration.
3. `zeph_config::types::security::CapabilityScopesConfig` — TOML schema,
   build-time glob resolution, `strict` and `default_scope` semantics.
4. `crates/zeph-core/src/agent/trajectory.rs` —
   `TrajectorySentinel`, `RiskSignal`, `RiskLevel`, `RiskAlert`,
   `TrajectorySentinelConfig`. Stored on `SecurityState`.
5. `advance_turn` invocation site in the agent loop, ordered before gate.
6. Sentinel hookups in `PolicyGateExecutor`, `ScopedToolExecutor`,
   `agent/tool_execution/sanitize.rs`, dispatch path, and subagent
   builder (`spawn_child`).
7. `ToolError::OutOfScope`, audit entries with
   `scope_at_definition` / `scope_at_dispatch`, and `error_category`
   stable strings (`out_of_scope`, `trajectory_alert`,
   `trajectory_critical`, `trajectory_auto_recover`).
8. CLI: `--scope <task_type>`; slash commands `/scope <name>`,
   `/scope reset`, `/trajectory show`, `/trajectory reset`.
9. TUI: status line surfaces *active scope* and *risk level* (one
   colour-coded chip each).
10. Config wizard (`--init`) + `--migrate-config` step for new keys.
11. Tests:
    - unit (scope admits/rejects, dead-pattern rejection,
      namespace-collision rejection, decay monotonicity per-turn,
      `advance_turn` ordering, auto-recover, signal weights);
    - integration (`PolicyGate` + `Scope` + `Sentinel` interleaved,
      subagent inheritance);
    - property-based (random Phase-1 signal traces never produce
      NaN/negative score and stay below `score_max`);
    - regression (Critical-deny followed by 16 idle turns recovers).
12. Live testing playbook (`.local/testing/playbooks/security-capability-governance.md`)
    and a row in `coverage-status.md`.
13. CHANGELOG entry under `[Unreleased]`.

### Phase 2 — follow-on (deferred)

- Auto-routing of scope from the complexity classifier (#3563 RL component).
- Confirmation-required mode at `High` risk.
- Sentinel weights learned online from operator overrides.
- `NovelTool` signal, gated on persistent per-installation novelty store.
- Cross-mode parity tests (CLI / TUI / Telegram) for `/scope` and
  `/trajectory`.

### Phase 3 — CapSeal + SUDP (#3569)

- Audit pass over every `VaultProvider::get_secret` caller; classify by op.
- Define `ops::*` typestate marker types per call-site cluster.
- Implement `VaultBroker` alongside `VaultProvider`. Migrate clusters one
  at a time, deleting `get_secret` for that key only after the last caller
  is migrated.
- Extend `AuditEntry` with `secret_ref` (the *reference*, not the value).

## Known Gaps

These are accepted Phase-1 limitations, intentionally documented so they
are not rediscovered as bugs:

- **Aggregate-attack window between auto-recovers (R1 residual).** Between
  hard resets, an attacker who paces multiple sub-`>= 1.5` signals can keep
  the score above `critical_at` until the turn-count cap fires. With
  defaults, this yields a 16-turn-deny / 1-turn-allow cycle until reset.
  Phase 2 introduces an optional `aggregate_per_turn_cap` so an operator
  can shorten the deny window without changing per-signal weights.
- **Provisional dynamic-namespace globs (R2 residual).** An `mcp:` glob
  that never resolves (server permanently unreachable) silently behaves as
  a deny-all for that namespace. The TUI status line surfaces unresolved
  provisional patterns so operators can spot stuck configurations, but
  there is no automatic escalation.
- **`NovelTool` deferred.** Phase 1 cannot detect first-time-tool-use
  signals without persistent per-installation state. Re-entering Phase 2
  scope behind `security.trajectory.novelty_signal`.
- **High risk does not require confirmation in Phase 1.** Allow decisions
  at `High` are forwarded with an audit tag but not blocked. Phase 2 adds
  an optional confirmation flow.

## Acceptance Criteria

- `cargo nextest run -p zeph-tools -E 'test(scope)'` passes.
- `cargo nextest run -p zeph-core -E 'test(trajectory)'` passes.
- A scope config containing a glob that matches zero registered tools
  produces a fatal startup error with `ScopeError::DeadPattern`.
- A live session with `--scope research` does NOT expose `builtin:edit` or
  `builtin:shell` in the LLM tool list; an LLM-emitted `builtin:shell`
  call returns `OutOfScope` and the audit log contains a row with
  `error_category = "out_of_scope"`.
- A synthetic trace of 6 `VigilFlagged(High)` signals over 8 turns drives
  the sentinel to `Critical`; the next `PolicyGate` allow is downgraded
  to `Deny { trace: "trajectory_critical" }`.
- After 16 consecutive turns at `Critical` with **no** new high-weight
  signal, the next `advance_turn` exits Critical, and the audit log
  contains `error_category = "trajectory_auto_recover"`.
- `/clear`, `/scope reset`, and `/trajectory reset` all reset the
  sentinel.
- A subagent spawned while the parent is at `Elevated` initialises with a
  damped, non-zero starting score.
- An MCP server attempting to register `read` (which collides with
  `builtin:read`) is rejected at registration; the namespaced id
  `mcp:hostile/read` is admitted only by an explicit `mcp:hostile/*` glob.
- Doc-tests on every new `pub` item;
  `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc
  --no-deps -p zeph-tools -p zeph-core` clean.
- CapSeal section is intentionally code-free; Phase 3 work will live
  behind a new spec (proposed: `010-7-capseal-vault-broker.md`).
