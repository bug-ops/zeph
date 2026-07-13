---
aliases:
  - ORCH Ensemble-Merge SRS
  - Deterministic Verifier Ensemble SRS
  - SRS 5912
tags:
  - sdd
  - srs
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/073-orch-ensemble-merge/brd]]"
  - "[[specs/073-orch-ensemble-merge/spec]]"
  - "[[specs/073-orch-ensemble-merge/nfr]]"
---

# SRS: ORCH Deterministic Verifier Ensemble-Merge (GitHub #5912)

ISO/IEC/IEEE 29148:2018 compliant. Requirements use EARS notation. Technical basis: architect
handoffs `.local/handoff/2026-07-13T19-47-29-architect.md` (base plan),
`.local/handoff/2026-07-13T19-54-56-architect.md` (S1/S2/S3+M1/M2 revision),
`.local/handoff/2026-07-13T20-00-06-architect.md` (S4/M5 revision, final), and critic
handoffs `.local/handoff/2026-07-13T19-50-08-critic.md`,
`.local/handoff/2026-07-13T19-58-40-critic.md`, `.local/handoff/2026-07-13T20-02-30-critic.md`
(final verdict: minor / approved).

## 1. Scope

This SRS specifies PR-1 of the ORCH ensemble-merge capability: N-fold parallel plan-verification
via configured provider members, a pure deterministic binary-majority merge function, quorum-based
graceful degradation to the existing single-provider path, and a telemetry-only per-member EMA
tracker. It resolves all `[NEEDS CLARIFICATION]` items from the original research spec
(`.local/specs/054-orch-ensemble-merge-discrete-choice/spec.md`) that are in scope for PR-1, and
folds in every correction from the three-round architect/critic review (S1-S4, M1-M7).

---

## 2. Ensemble Dispatch

### FR-001: N-Fold Parallel Verification Dispatch

**WHEN** `SchedulerAction::Verify { task_id, output }` is handled
(`crates/zeph-core/src/agent/scheduler_loop.rs:459`) **AND**
`[orchestration.ensemble].enabled == true` **AND** `[orchestration.ensemble].verify == true`,
**THE SYSTEM SHALL** dispatch one `chat_typed::<VerificationResult>` call per configured
ensemble member, in parallel, via `futures::future::join_all`, awaited inline on the current
(already-supervised) scheduler-loop task — introducing no new `tokio::spawn()` call site.

**WHEN** either `enabled == false` **OR** `verify == false`,
**THE SYSTEM SHALL** dispatch exactly the current single-call `PlanVerifier::verify()` path,
unchanged (BG-02).

### FR-002: Per-Member Timeout

**THE SYSTEM SHALL** wrap each member's `chat_typed` call in its own `tokio::time::timeout`
using `member_timeout_secs` (config default: the existing `verifier_timeout_secs` value),
consistent with `PlanVerifier`'s existing per-call timeout pattern
(`crates/zeph-orchestration/src/verifier.rs:162`).

### FR-003: Errored/Timed-Out Members Are Excluded Ballots, Not Fail-Open Votes

**WHEN** a member's `chat_typed` call returns `Err(_)` or times out,
**THE SYSTEM SHALL** exclude that member from the ballot set entirely — it contributes no vote
to the majority and no value to the confidence mean.

**THE SYSTEM SHALL NOT** apply `PlanVerifier`'s existing per-member `fail_open()` (`{complete:
true, confidence: 0.0}`, `verifier.rs:107-113`) to an individual ensemble member's error. That
fail-open construction is reserved for exactly one place: the ensemble-level fallback when
quorum is not met (FR-008). Applying it per-member would (a) cast a spurious `complete=true`
vote skewing the majority, and (b) drag the confidence mean toward `0.0` for a member that never
actually voted. (Critic finding M6.)

**Acceptance test:** N=3 configured members, 1 member errors — the merge proceeds over the 2
responding members; the resulting `VerificationResult.confidence` is computed only from the
winning-side members among those 2 responders, never touched by the errored member's absence.

---

## 3. Deterministic Merge

### FR-004: Binary Ballot on `complete`

**THE SYSTEM SHALL** treat each responding member's `VerificationResult.complete: bool` field as
its ballot. **THE SYSTEM SHALL NOT** vote on the 4-way ordinal `GapSeverity` in PR-1 — that merge
shape ties far more often than a binary vote (independent of ensemble parity) and makes a
tie-break load-bearing for the common case rather than an edge case, defeating the ensemble's
denoising purpose (critic finding S1). The ordinal/severity-class merge is a documented future
phase, not a v1 default.

### FR-005: Majority Vote, No Ties By Construction

**THE SYSTEM SHALL** compute the merged `complete` value as the majority of responding members'
`complete` ballots. **THE SYSTEM SHALL** rely on the config-load-time odd/≥3 validation (FR-014)
so that a full-response round always produces a strict majority with no tie.

**WHEN** timeouts/errors (FR-003) leave an even number of responding members **AND** that even
count still meets quorum (FR-007/FR-008),
**THE SYSTEM SHALL** apply the fail-safe tie-break: treat the merged result as `complete = false`
(prefer replan over silent accept). This is a documented degenerate path, not the primary
mechanism — it is only reachable via partial failure, never via a full-response round.

### FR-006: Winning-Side Gap Union and Confidence Mean

**THE SYSTEM SHALL** set the merged `gaps: Vec<Gap>` to the verbatim union of gap lists from
members whose `complete` ballot matches the winning side. **THE SYSTEM SHALL NOT** attempt
semantic reconciliation of differing gap *text* across members — that would itself require an
LLM call, reintroducing non-determinism (documented open question, deferred).

**THE SYSTEM SHALL** set the merged `VerificationResult.confidence` to the arithmetic mean of the
winning-side members' own self-reported `confidence` values (each member's
`chat_typed::<VerificationResult>` already returns `confidence: f64`,
`crates/zeph-orchestration/src/verifier.rs:96-103`).

**THE SYSTEM SHALL NOT** write `agreement_ratio` (winning-side vote count / responding member
count) into `VerificationResult.confidence` or into any other field of `VerificationResult`.
`agreement_ratio` lives exclusively on the internal `MergeOutcome` type and the telemetry surface
(FR-012).

> **Rationale (critic finding S4, blocking, resolved):** the existing downstream gate is
> `should_replan = !complete && confidence < completeness_threshold && has_actionable_gap`
> (`scheduler_loop.rs:492-500`, default `completeness_threshold = 0.7`,
> `crates/zeph-config/src/experiment.rs:147`). Writing `agreement_ratio ∈ [0.5, 1.0]` into
> `confidence` inverts this gate: a 3-of-3 unanimous incomplete+critical-gap verdict would yield
> `agreement_ratio = 1.0`, `1.0 < 0.7` is false, so **no replan fires** — dropping a confirmed
> critical gap — while a 2-of-3 split (`agreement_ratio = 0.667`) would trigger replan, exactly
> backwards. Computing `confidence` as the mean of winning-side members' own reported confidence
> instead preserves the gate's original semantic (LLM-reported certainty) with **zero code
> changes required in `scheduler_loop.rs` or `plan.rs`**.

### FR-007: Pure, Deterministic, Unit-Testable Merge Function

**THE SYSTEM SHALL** implement the merge as a pure function `merge(ballots: &[Ballot]) ->
MergeOutcome` with no I/O, no LLM call, and no randomness — a total function of its input
ballots. **THE SYSTEM SHALL** make this function unit-testable in complete isolation, with
hand-constructed `Ballot` fixtures and no provider/network dependency.

---

## 4. Quorum and Graceful Degradation

### FR-008: Derived Quorum, Fallback to Single-Provider Path

**THE SYSTEM SHALL** derive `quorum = members.len() / 2 + 1` (strict majority) — **THE SYSTEM
SHALL NOT** expose `quorum` or `min_quorum` as an independent config field (critic finding M5,
resolved: removes an operator footgun and keeps the odd-size no-tie guarantee structural).

**WHEN** fewer than `quorum` members respond (errors/timeouts, FR-003),
**THE SYSTEM SHALL** fall back to the existing single-primary `verify_provider` path
(`PlanVerifier::verify()`, unmodified), and **THE SYSTEM SHALL** apply the existing fail-open
behavior (`VerificationResult{complete: true, confidence: 0.0}`) if that single-provider call
also fails. **THE SYSTEM SHALL NOT** introduce any new failure mode beyond what exists today —
the ensemble path degrades exactly to current single-agent behavior, never fail-closed.

### FR-009: `ensemble_degraded` Observability

**WHEN** the quorum fallback (FR-008) fires,
**THE SYSTEM SHALL** increment an `ensemble_degraded` counter/metric and emit a `warn!`-level log
so a misconfigured `members` list (e.g. a persistently-down member, a typo'd provider name) does
not silently degrade every ensemble decision to single-provider behavior without an operator ever
noticing (critic finding M1).

---

## 5. Telemetry-Only EMA Tracker (No Participation Gating)

### FR-010: Full Ensemble, No Subset Selection

**THE SYSTEM SHALL** dispatch to **every** configured member on every ensemble-treated verify
decision — `size == members.len()` always. **THE SYSTEM SHALL NOT** implement or call a
`select_subset(k)` function in PR-1.

> **Rationale (critic finding S2, resolved):** rewarding agreement-with-the-merged-majority and
> then using that same reward to gate which members are selected for the next round creates a
> self-reinforcing consensus-collapse loop — members that most often agree with the majority gain
> EMA and get selected more, dissenting (possibly correct) members lose EMA and get pruned from
> selection, and the panel converges to the k most-agreeable members, defeating the diversity the
> ensemble exists to exploit. Running the full ensemble every round with no selection pressure
> closes this loop entirely and is also a net PR-1 scope reduction.

### FR-011: EMA Tracker Records Agreement, Telemetry Only

**THE SYSTEM SHALL** implement a new, pure-EMA (`score ← α·outcome + (1−α)·score`), deterministic
per-member tracker in `zeph-orchestration`, distinct from `zeph-llm`'s Beta/Thompson-based RAPS
`ReputationTracker` (`crates/zeph-llm/src/router/reputation.rs:41`, `pub(crate)`, coupled to
`super::thompson::BetaDist`, `ema_reputation_factor()` is a Beta *mean* not a true EMA) and
distinct from `AdaptOrch`'s Thompson-sampling topology bandit
(`crates/zeph-orchestration/src/adaptorch.rs:383`, stochastic by construction, violates
determinism). **THE SYSTEM SHALL** record, per member, whether that member's ballot agreed with
the merged majority. **THE SYSTEM SHALL NOT** use this tracker's score to gate which members
participate in any round (see FR-010) — it is diagnostics/telemetry only in PR-1.

### FR-012: Ensemble Stats Surface

**THE SYSTEM SHALL** expose, via CLI/TUI stats and metrics: each member's current EMA score,
observation count, the internal `agreement_ratio` from the most recent merges, and the
`ensemble_degraded` counter (FR-009). This satisfies the original research spec's FR-008
("surface disagreement distinctly") as an observability-only signal, not a behavioral input to
`replan()`.

---

## 6. Configuration and Validation

### FR-013: `[orchestration.ensemble]` Config Section

**THE SYSTEM SHALL** expose:

```toml
[orchestration.ensemble]
enabled = false              # master opt-in (default OFF -> single-agent path unchanged)
verify = false                # PR-1's only target: ensemble-treat the per-task verify() step
members = ["fast", "quality", "balanced"]   # names -> [[llm.providers]]; MUST be odd, >= 3, no duplicates when enabled
ema_alpha = 0.3               # EMA smoothing -- TELEMETRY ONLY (does not gate participation)
ema_decay = 0.95              # session decay toward prior -- telemetry only
min_observations = 5          # cold-start gate before a member's EMA is considered warmed up -- telemetry only
member_timeout_secs = 0       # 0 = fall back to the existing verifier_timeout_secs default
```

**THE SYSTEM SHALL NOT** expose `size` or `min_quorum` as config fields — both are derived
(FR-010, FR-008). Ensemble members SHOULD run at `temperature = 0` (documented precondition,
not enforced in code) to minimize sampling-induced ballot variance (see NFR determinism scoping).

### FR-014: Load-Time Validation — Odd, ≥3, No Duplicates

**WHEN** `[orchestration.ensemble].enabled == true` **AND** `verify == true`,
**THE SYSTEM SHALL** validate, at config load time (alongside the existing
`completeness_threshold` range check in `crates/zeph-config/src/loader.rs::validate_orchestration`):

1. `members.len()` is odd and `>= 3` — otherwise `Err(ConfigError::Validation(...))`, fail-fast,
   never reaching the merge path with an even or short list.
2. `members` contains no duplicate provider names (after case-sensitive exact-string comparison)
   — otherwise `Err(ConfigError::Validation(...))`. A list like `["fast", "fast", "fast"]` passes
   the odd/≥3 count but is one provider voting three times; under the `temperature = 0`
   precondition those three calls are near-identical, so agreement is trivially ~1.0 and the
   ensemble denoises nothing (critic finding M7).

**WHEN** either `enabled == false` **OR** `verify == false`,
**THE SYSTEM SHALL NOT** apply the FR-014 checks — an unused `members` list (e.g. left over from
a prior config, or not yet configured) does not block startup.

### FR-015: Bootstrap Resolution and Wiring

**THE SYSTEM SHALL** resolve `[orchestration.ensemble].members` into `Vec<AnyProvider>` at
bootstrap via the existing `create_named_provider(name, config)` seam
(`src/bootstrap/provider.rs:279`), mirroring `Bootstrap::build_verify_provider`
(`src/bootstrap/mod.rs:1916`). **THE SYSTEM SHALL** store the resolved providers on a new
`ensemble_members: Vec<AnyProvider>` field on `OrchestrationState`
(`crates/zeph-core/src/agent/state/mod.rs`, sibling to the existing
`verify_provider: Option<AnyProvider>` field at line 653), threaded via a builder method mirroring
the existing pattern used for `verify_provider`.

### FR-016: Per-Member Usage Metering

**THE SYSTEM SHALL** record LLM token usage for every member's `chat_typed` call made as part of
an ensemble verify decision. This closes an existing gap: `PlanVerifier::verify()` performs no
usage/cost metering today, so the N-fold ensemble fan-out would otherwise multiply an
already-invisible cost with zero budget visibility (critic finding M1).

---

## 7. Mandatory Integration Points

### FR-017: CLI/TUI, `--init`, `--migrate-config`

**THE SYSTEM SHALL** provide:
1. The `[orchestration.ensemble]` config section (FR-013).
2. A `--init` wizard step prompting for ensemble opt-in and member list when configuring
   orchestration.
3. No `--migrate-config` step is required to *add* the new section (new optional table with all
   defaults preserving current behavior) — **THE SYSTEM SHALL** confirm this explicitly rather
   than silently omitting the migration step.
4. A CLI/TUI stats surface for ensemble telemetry (FR-012), including a background-status
   indicator (spinner) while N-fold verification is in flight, per the TUI Rules mandatory
   status-indicator requirement.
5. A testing playbook and coverage-status rows per the project's mandatory integration points.

---

## 8. Deferred Requirements (Acknowledged)

### FR-D-01: EMA-Gated Subset Selection

Deferred (BRD §5; FR-010/FR-011). Requires a ground-truth (not self-consistency) reward signal
before it is safe to re-introduce without risking consensus collapse (critic finding S2).

### FR-D-02: Ordinal/Severity-Class Merge

Deferred (BRD §5; FR-004). A 4-way plurality vote with a severity-conservative tie-break was
shown to defeat denoising in the common case (critic finding S1); a future phase may revisit this
with a different tie-break policy.

### FR-D-03: Weighted Majority Vote

Deferred (BRD §5). EMA governs cost/telemetry only in PR-1; using it to weight votes as well
would double-count reputation.

### FR-D-04: Phase-2 Full-Node Subagent Fan-Out

Deferred (BRD §5). `SchedulerAction::Spawn` × N for one `TaskNode`, merged on the `TaskEvent`
completion channel — sketched by the architect but not built in PR-1.

### FR-D-05: `zeph-sanitizer` / `zeph-tools` Ensemble Consumers

Deferred (BRD §5). Cross-crate dependency inversion required; out of scope for PR-1.

### FR-D-06: Whole-Plan Verification Ensemble

Deferred (BRD §5). `verify_plan()` / `replan_from_plan()` stay single-provider in v1.

### FR-D-07: Gap-Text Semantic Reconciliation

Deferred (FR-006). Merging differing free-text gap descriptions across winning-side members has
no deterministic rule; v1 unions them verbatim.

### FR-D-08: Ground-Truth Reward Signal

Deferred (FR-011). A prerequisite to ever safely re-introducing FR-D-01; requires a delayed
correctness signal (e.g., a later replan revealing a verify verdict was wrong), which does not
currently exist anywhere in the orchestration subsystem.

### FR-D-09: Per-DAG Ensemble Cost Cap

Deferred. v1 relies on FR-016/FR-009 (warn-only observability); a hard per-DAG or global cap on
the number of ensemble-eligible decisions is a documented future option, not built in PR-1.

---

## 9. Traceability Matrix

| Requirement | BRD Goal | Architect/Critic Source |
|-------------|----------|--------------------------|
| FR-001, FR-002 | BG-01, BG-04 | Base architect plan, decision (4); critic-confirmed spec-039 compliance |
| FR-003 | BG-02 | Critic finding M6 (final round, resolved) |
| FR-004, FR-005 | BG-01 | Critic finding S1; architect revision 1 |
| FR-006 | BG-01, BG-02 | Critic findings S4 (blocking) and S1; architect revision 2 (final) |
| FR-007 | BG-04 | Critic finding S3; architect revision 1 |
| FR-008, FR-009 | BG-02, BG-03 | Base architect plan, decision (6); critic finding M1 |
| FR-010, FR-011 | BG-01, BG-04 | Critic finding S2; architect revision 1 |
| FR-012 | BG-03 | Original research spec FR-008; critic finding M1 |
| FR-013, FR-014 | BG-01, BG-02 | Critic finding M5 (final round, resolved); critic finding M7 |
| FR-015 | BG-04 | Critic finding M2 |
| FR-016 | BG-03 | Critic finding M1 |
| FR-017 | BG-01 | CLAUDE.md mandatory integration points; TUI Rules |
| FR-D-01..09 | (deferred) | BRD §5; critic findings S1/S2, original research spec open questions |
