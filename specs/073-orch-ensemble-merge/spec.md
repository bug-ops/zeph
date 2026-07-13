---
aliases:
  - ORCH Ensemble-Merge
  - Deterministic Verifier Ensemble
  - Spec 5912
tags:
  - sdd
  - spec
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[specs/073-orch-ensemble-merge/brd]]"
  - "[[specs/073-orch-ensemble-merge/srs]]"
  - "[[specs/073-orch-ensemble-merge/nfr]]"
  - "[[specs/073-orch-ensemble-merge/plan]]"
  - "[[001-system-invariants/spec]]"
  - "[[009-orchestration/spec]]"
  - "[[039-background-task-supervisor/spec]]"
  - "[[024-multi-model-design/spec]]"
issues:
  - "#5912"
---

# Spec 073 — ORCH Deterministic Verifier Ensemble-Merge (arXiv:2602.01797)

> [!info]
> N configured provider members independently classify the same completed-task output in
> parallel; a pure, deterministic binary-majority merge produces the single `VerificationResult`
> `PlanVerifier`'s existing `replan()` gate already consumes. Opt-in, default OFF, zero new
> `tokio::spawn` sites, zero change to `VerificationResult`'s shape or the downstream replan gate.
> This spec is the authoritative implementation contract, derived from the three-round
> architect/critic design review (final critic verdict: **minor / approved**,
> `.local/handoff/2026-07-13T20-02-30-critic.md`). It formalizes that design into traceable
> requirements; it does not re-derive the architecture. Resolves the WHAT/HOW gap left open by
> the original research spec (`.local/specs/054-orch-ensemble-merge-discrete-choice/spec.md`,
> GitHub #5912).

## Sources

### External
- [ORCH: many analyses, one merge — a deterministic multi-agent orchestrator for discrete-choice reasoning with EMA-guided routing](https://arxiv.org/abs/2602.01797) (arXiv:2602.01797, Zhou & Chan, 2026-02-02); also in [Frontiers in Artificial Intelligence](https://www.frontiersin.org/journals/artificial-intelligence/articles/10.3389/frai.2026.1748735/full)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-orchestration/src/verifier.rs` | `PlanVerifier::verify()` (`:159`), single `chat_typed::<VerificationResult>` call with per-call `tokio::time::timeout` (`:162`); `VerificationResult { complete: bool, gaps: Vec<Gap>, confidence: f64 }` (`:96-103`); `fail_open()` = `{complete:true, confidence:0.0}` (`:107-113`); `Gap { description, severity: GapSeverity }` (`GapSeverity ∈ {Critical, Important, Minor}`) |
| `crates/zeph-core/src/agent/scheduler_loop.rs` | `SchedulerAction::Verify` handling (`:459-535`) — already inline-awaited on the supervised scheduler-loop task; `should_replan = !result.complete && result.confidence < f64::from(threshold) && has_critical_or_important_gap` (`:492-500`) — the unchanged downstream gate this spec's confidence semantics must preserve |
| `crates/zeph-orchestration/src/scheduler/mod.rs` | `SchedulerAction` enum (`Spawn`, `RunInline`, `Verify`, `VerifyPredicate`, `:63-`) — command-emitter, not a spawner; the caller executes actions |
| `crates/zeph-config/src/experiment.rs` | `OrchestrationConfig.verify_provider: ProviderName` (`:303`), `.completeness_threshold: f32` (`:331-332`, default `0.7`, `:147`) |
| `crates/zeph-config/src/loader.rs` | `validate_orchestration()` — existing `completeness_threshold ∈ [0.0,1.0]` range check; new odd/≥3/no-duplicate `members` validation is added alongside it |
| `crates/zeph-llm/src/router/reputation.rs` | RAPS `ReputationTracker` (`:41`), `pub(crate) models` field (`:42`), coupled to `super::thompson::BetaDist` (`:25,31`), `ema_reputation_factor()` (`:133`) is a Beta *mean*, not a true EMA — rejected as a reuse target |
| `crates/zeph-orchestration/src/adaptorch.rs` | `AdaptOrch` Thompson-sampling topology bandit — `rng.lock(); dist.sample(&mut *rng)` (`:383,386`), stochastic by construction — rejected as a reuse target (violates determinism) |
| `src/bootstrap/provider.rs` | `create_named_provider(name, config) -> Result<AnyProvider, _>` (`:279`) — the provider-name resolution seam `ensemble.members` resolves through |
| `src/bootstrap/mod.rs` | `Bootstrap::build_verify_provider()` (`:1916-1932`) — sibling pattern the new ensemble-members resolution mirrors |
| `crates/zeph-core/src/agent/state/mod.rs` | `OrchestrationState.verify_provider: Option<AnyProvider>` (`:653`) — sibling field the new `ensemble_members: Vec<AnyProvider>` field is added next to |
| `crates/zeph-orchestration/src/error.rs` | `OrchestrationError` (`#[non_exhaustive]`, `:37-38`) — extension point for a hard ensemble-config error, if needed |
| `crates/zeph-orchestration/src/graph.rs` | `TaskNode` (`:378-`) — existing `#[serde(default)]` `Option<...>` per-node config field pattern (`token_budget_cents`, `network_scope`, `asset_sensitivity`, `execution_mode`, `verify_predicate`); phase-2 per-node ensemble opt-in would follow this pattern (not built in PR-1) |

---

## 1. Overview

### Problem Statement

`PlanVerifier::verify()` gates Zeph's most expensive orchestration cycle — `replan()` — on the
verdict of exactly one LLM call. No existing Zeph orchestration primitive (`AgentRouter`,
`AdaptOrch`, `PlanVerifier`, `Aggregator`) implements ORCH's "N independent judgments +
deterministic merge" pattern, per the prior research audit
(`.local/specs/054-orch-ensemble-merge-discrete-choice/spec.md`). A single noisy verdict is
indistinguishable today from a genuine gap, and there is no way to trade a bounded, opt-in cost
increase for higher confidence specifically on this decision.

### Goal

An operator can opt a deployment into ensemble-verified plan verification. When enabled, the
existing `SchedulerAction::Verify` handler dispatches N parallel `chat_typed::<VerificationResult>`
calls (one per configured provider member) instead of one, and a pure, deterministic
binary-majority merge produces the single `VerificationResult` that `replan()` already consumes —
with zero change to that downstream gate's code or semantics. Default is OFF; misconfiguration or
partial member failure degrades gracefully to today's exact single-provider behavior, never
fail-closed.

### Out of Scope

See `[[specs/073-orch-ensemble-merge/brd]]` §5 for the full list with rationale. Summary:
EMA-gated subset selection, ordinal/severity-class merge, weighted voting, ground-truth reward
signal, phase-2 full-node subagent fan-out, `zeph-sanitizer`/`zeph-tools` consumers, whole-plan
verification ensembling, and benchmark reproduction are all explicitly deferred.

Full requirement-level detail: `[[specs/073-orch-ensemble-merge/srs]]`. Quality targets:
`[[specs/073-orch-ensemble-merge/nfr]]`.

---

## 2. Functional Requirements

See `[[specs/073-orch-ensemble-merge/srs]]` for the complete EARS-notation requirement set
(FR-001 through FR-017 plus FR-D-01..09 deferred) and traceability matrix. Summary:

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001/002 | N-fold parallel `chat_typed` dispatch via inline `join_all`, per-member timeout | must |
| FR-003 | Errored/timed-out members excluded from ballot, not fail-open votes (M6) | must |
| FR-004/005 | Binary majority vote on `complete`; odd-size validation eliminates ties structurally | must |
| FR-006 | Winning-side gap union; `confidence` = mean of winning-side self-reported confidence, NOT `agreement_ratio` (S4) | must |
| FR-007 | Merge is a pure, unit-testable function | must |
| FR-008/009 | Derived quorum, graceful fallback to single-provider path; `ensemble_degraded` metric | must |
| FR-010/011 | Full ensemble every round, no `select_subset`; EMA telemetry-only (S2) | must |
| FR-012 | CLI/TUI stats surface | must |
| FR-013/014 | Config schema; load-time odd/≥3/no-duplicate validation (M5, M7) | must |
| FR-015 | Bootstrap resolution + wiring | must |
| FR-016 | Per-member usage metering (M1) | must |
| FR-017 | Mandatory integration points | must |

---

## 3. Architecture / Design

### 3.1 Dispatch Seam (unchanged core mechanism)

`SchedulerAction::Verify { task_id, output }` is already handled inline (awaited, not spawned) in
`crates/zeph-core/src/agent/scheduler_loop.rs:459-535`, on a task already supervised by the
scheduler-loop's own execution context. The ensemble path is a variant branch inside this same
handler: when `[orchestration.ensemble].enabled && verify`, build N verifier calls against
`OrchestrationState.ensemble_members` instead of the single `verify_provider`; otherwise, the
existing single-call path runs completely unmodified.

### 3.2 Data Flow

```
SchedulerAction::Verify{task_id, output}
        │
        ▼
enabled&&verify? ──No──> PlanVerifier::verify() [unchanged single-provider path]
        │Yes
        ▼
join_all(members.map(|m| timeout(member_timeout_secs, m.chat_typed::<VerificationResult>(...))))
        │
        ▼
responses: Vec<Result<VerificationResult, LlmError|Timeout>>
        │
        ▼
responded = filter Ok(_) ──┐
                            │
        responded.len() < quorum? ──Yes──> ensemble_degraded++, warn!, fall back to
        │No                                 PlanVerifier::verify() [existing fail-open included]
        ▼
ballots = responded.map(|r| Ballot{member, complete: r.complete, confidence: r.confidence, gaps: r.gaps})
        │
        ▼
merge(ballots) -> MergeOutcome{ complete, winning_gaps, merged_confidence, agreement_ratio, tie_broken }
        │                                                        │
        ▼                                                        ▼
VerificationResult{ complete, gaps: winning_gaps,      EnsembleTracker.record(member, agreed)
                     confidence: merged_confidence }    (telemetry only — no gating)
        │                                                        │
        ▼                                                        ▼
   replan() gate — UNCHANGED                          CLI/TUI stats + metrics (agreement_ratio,
                                                        ensemble_degraded, per-member EMA)
```

### 3.3 Key Types (illustrative — exact signatures are an implementation detail of the plan)

- `EnsembleConfig { enabled: bool, verify: bool, members: Vec<String>, ema_alpha: f64, ema_decay: f64, min_observations: u32, member_timeout_secs: u64 }` — new, in `zeph-config`.
- `Ballot { member: String, complete: bool, confidence: f64, gaps: Vec<Gap> }` — new, in `zeph-orchestration`.
- `MergeOutcome { complete: bool, gaps: Vec<Gap>, merged_confidence: f64, agreement_ratio: f64, tie_broken: bool }` — new, internal only; `agreement_ratio` and `tie_broken` never cross into `VerificationResult`.
- `fn merge(ballots: &[Ballot]) -> MergeOutcome` — pure, deterministic, unit-tested in isolation (NFR-DR-01).
- `EnsembleTracker { scores: HashMap<String, EmaEntry>, alpha: f64, decay: f64, min_obs: u32 }` with `record(member, agreed: bool)`, `ema(member) -> Option<f64>` — new, in `zeph-orchestration`, telemetry-only (no `select_subset`).
- `OrchestrationState.ensemble_members: Vec<AnyProvider>` — new field, sibling to `verify_provider`.

### 3.4 Why a New Tracker, Not RAPS or `AdaptOrch`

- **RAPS `ReputationTracker`** (`zeph-llm/src/router/reputation.rs:41`) is Beta/Thompson-coupled
  (`pub(crate) models`, depends on `super::thompson::BetaDist`), semantically provider-scoped
  (not agent/member-scoped), and its `ema_reputation_factor()` is a Beta *mean*, not a true EMA.
  Generalizing it would drag Thompson-sampling machinery into a component that must be
  deterministic, and would require a cross-crate refactor of a load-bearing, race-fix-heavy
  component for this feature's first PR.
- **`AdaptOrch`** (`adaptorch.rs`) is Thompson **sampling** — stochastic by construction — and
  selects DAG topology, not per-decision agent ballots. It is architecturally the wrong shape and
  violates NFR-DR-01/02.
- **A new, small, deterministic pure-EMA tracker in `zeph-orchestration`** borrows both existing
  trackers' *design lessons* (cold-start gate via `min_observations`, decay-toward-prior, atomic
  score updates) without inheriting their type-level coupling. This is an accepted, documented
  DRY trade-off per this project's pre-1.0 "extract to `zeph-common` only at the third consumer"
  rule (NFR-MA-01).

---

## 4. Key Invariants

### Always (without asking)

- `[orchestration.ensemble].enabled = false` (default) reproduces the exact current
  single-provider `PlanVerifier::verify()` code path, byte-for-byte (NFR-CO-01).
- `VerificationResult` gains no new field — `agreement_ratio` and per-member EMA scores live only
  on `MergeOutcome`/`EnsembleTracker` and the telemetry surface, never in the type `replan()`
  consumes (S4, NFR-CO-02).
- Merged `confidence` is always the mean of the **winning-side** members' own self-reported
  `confidence` values — never `agreement_ratio` (FR-006).
- The merge function `merge(ballots) -> MergeOutcome` is pure: no I/O, no LLM call, no
  randomness (FR-007, NFR-DR-01).
- Errored/timed-out members are excluded from the ballot set entirely — never counted as a vote,
  never contributing to the confidence mean (FR-003, M6).
- `members.len()` is validated odd and `>= 3`, with no duplicate provider names, at config load
  time whenever `enabled && verify` (FR-014, M5+M7).
- `size` and `quorum`/`min_quorum` are always derived from `members.len()` — never independent
  config knobs (FR-008, FR-013, M5).
- The N-fold fan-out is always `futures::future::join_all` awaited inline on the existing
  supervised scheduler-loop task — never a new `tokio::spawn()` call site (NFR-AS-01).
- Every ensemble member `chat_typed` call is independently wrapped in its own
  `tokio::time::timeout(member_timeout_secs, ...)` (FR-002, NFR-AS-03).
- Below-quorum responses always fall back to the existing single-provider `verify_provider` path,
  and to the existing fail-open result if that also fails (FR-008, NFR-RE-01).
- Every ensemble member call records LLM token usage (FR-016, NFR-OB-02).

### Ask First

- Re-introducing `select_subset(k)` / EMA-gated participation — requires a ground-truth reward
  signal to exist first (FR-D-01); re-opening this without one risks the consensus-collapse loop
  identified in critic finding S2.
- Adding the ordinal/severity-class merge as an alternative or replacement for the binary
  `complete` vote (FR-D-02) — requires an explicit tie-break policy redesign, since the 4-way
  vote was shown to defeat denoising (S1).
- Extending ensemble treatment to `verify_plan()`/`replan_from_plan()` (whole-plan verification,
  FR-D-06) or to `zeph-sanitizer`/`zeph-tools` decision points (FR-D-05) — both are architectural
  forks with cross-crate implications.
- Adding a hard per-DAG/global cap on ensemble-eligible decision count (FR-D-09) — v1 is
  warn-only via `ensemble_degraded`/usage metering; a hard cap changes failure semantics and
  needs its own design.

### Never

- **NEVER** write `agreement_ratio` into `VerificationResult.confidence` or any other
  `VerificationResult` field. This inverts the existing `should_replan` gate
  (`scheduler_loop.rs:492-500`) — a unanimous incomplete+critical-gap verdict would suppress
  replan while a disagreeing panel would trigger it, exactly backwards (S4).
- **NEVER** apply `PlanVerifier`'s per-member `fail_open()` construction to an individual
  ensemble member's error or timeout. That would cast a spurious `complete=true` vote and corrupt
  the confidence mean. `fail_open()` is reserved for the ensemble-level below-quorum fallback only
  (M6).
- **NEVER** allow a duplicate provider name in `members` to pass the odd/≥3 validation
  uncaught — deduplication (or rejection) must happen before the count is validated (M7).
- **NEVER** vote on the 4-way ordinal `GapSeverity` as the v1 merge target — this collapses split
  panels to their single most-pessimistic member and produces systematic false-positive replans
  (S1).
- **NEVER** let `EnsembleTracker`'s EMA score gate which members are dispatched to in a given
  round in PR-1 — this creates the consensus-collapse loop identified in S2. Full ensemble every
  round, no exceptions, until a ground-truth reward signal justifies re-opening this.
- **NEVER** introduce a new `tokio::spawn()` call site for the N-fold fan-out — per
  `[[039-background-task-supervisor/spec]]`'s binding NEVER section, inline `join_all` on the
  already-supervised scheduler-loop task is the only compliant mechanism for PR-1 (NFR-AS-01).
- **NEVER** hold a lock across any of the N concurrent `.await` points in the fan-out
  (NFR-AS-02).
- **NEVER** let the ensemble path introduce a new failure mode beyond today's fail-open
  behavior — below-quorum always degrades to the existing single-provider path, never to a hard
  error that blocks task completion (NFR-RE-01).

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `enabled = false` (default) | Exact current single-provider path; no ensemble code executes (NFR-CO-01) |
| `enabled = true, verify = false` | Ensemble machinery configured/resolved but the verify seam still uses the single-provider path — `verify` is the per-target activation flag |
| `members` has an even length or < 3 entries, ensemble active | `ConfigError::Validation` at load time — process fails to start with a descriptive message (FR-014) |
| `members` contains a duplicate provider name, ensemble active | `ConfigError::Validation` at load time (M7) |
| N=3, 1 member errors/times out | Merge proceeds over the 2 responders; errored member contributes no vote and no confidence value (M6, meets quorum=2) |
| N=3, 2 members error/time out | Below quorum (need ≥2 of 3) — falls back to single-provider `verify_provider` path; `ensemble_degraded` fires |
| N=5, timeouts leave an even (e.g. 2) responder count that still meets quorum | Degenerate tie path: fail-safe `complete = false` (prefer replan); documented as rare, not primary (FR-005) |
| 3-of-3 unanimous `complete=false` with a Critical gap | `agreement_ratio = 1.0` (telemetry only); merged `confidence` = mean of the 3 members' own reported confidence — `should_replan` behaves exactly as it would for a single low-confidence verdict, no inversion (S4 regression guard) |
| 2-of-3 split `complete=false` | Winning side (2 members) determines `complete`; `confidence` = mean of those 2 members' reported confidence; `agreement_ratio = 0.667` recorded to telemetry only |
| Misconfigured/persistently-down member causes repeated quorum fallback | `ensemble_degraded` counter increments and a `warn!` fires every time — never silent (M1) |
| Ensemble members not configured with `temperature = 0` | No enforcement in code; documented precondition only — ballot variance may be higher than expected, `agreement_ratio` becomes less interpretable as a disagreement signal (S3) |

---

## 6. Success Criteria

Implementation-facing checklist (business-facing criteria: `[[specs/073-orch-ensemble-merge/brd]]` §6):

- [ ] Default-off regression test: `enabled = false` reproduces the exact pre-feature single-provider code path
- [ ] Merge unit tests: pure `merge()` function tested in isolation with fixed `Ballot` fixtures, no LLM
- [ ] M6 acceptance test: N=3, 1 member errors, merge proceeds over 2 responders, confidence mean excludes the errored member
- [ ] M7 acceptance test: duplicate member names rejected at config load
- [ ] S4 regression test: 3-of-3 unanimous incomplete+critical vs. 2-of-3 split — confirm `should_replan` computation is not inverted
- [ ] Config validation tests: even-length, short (<3), and duplicate-name `members` all rejected at load
- [ ] Quorum fallback test: below-quorum responses fall back to single-provider path + fail-open; `ensemble_degraded` observed
- [ ] Zero new `tokio::spawn()` call sites: `.claude/rules/continuous-improvement.md` async-supervision scan count non-increasing
- [ ] `cargo +nightly fmt --check`, `cargo clippy --profile ci ... -D warnings`, `cargo nextest run ...`, and the rustdoc gate all pass per `.claude/rules/branching.md`
- [ ] `.local/testing/playbooks/orch-ensemble-merge.md` created (main-repo path)
- [ ] `.local/testing/coverage-status.md` rows added (main-repo path, status `Untested`)
- [ ] CLI/TUI ensemble stats surface verified live

---

## 7. Relationship to Existing Specs

| This spec | Existing spec | Relationship |
|-----------|---------------|---------------|
| `SchedulerAction::Verify` ensemble branch, `PlanVerifier` interaction | `[[009-orchestration/spec]]` §"Plan Verification" | Extends the existing verify/replan flow; does not change `VerificationResult`'s shape, `should_replan` gate, `max_replans_remaining`, or fail-open semantics |
| Inline `join_all` fan-out, zero new spawn sites | `[[039-background-task-supervisor/spec]]` | Compliance claim verified against the binding NEVER section — see NFR-AS-01 |
| `members` name-list → `[[llm.providers]]` resolution | `[[024-multi-model-design/spec]]` | Follows the mandated `*_provider`/name-list reference pattern — no inline provider config duplication |
| `EnsembleTracker` vs. RAPS `ReputationTracker` vs. `AdaptOrch` | `[[003-llm-providers/spec]]`, `[[009-orchestration/spec]]` §"AdaptOrch Topology Advisor" | Explicitly a new, separate, deterministic tracker — not a generalization of either existing mechanism (§3.4) |
| Original research/gap-audit spec | `.local/specs/054-orch-ensemble-merge-discrete-choice/spec.md` | This spec is the formal `/sdd` output resolving that spec's `[NEEDS CLARIFICATION]` items that are in-scope for PR-1 |

---

## 8. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[specs/073-orch-ensemble-merge/brd]] — Business case and success criteria
- [[specs/073-orch-ensemble-merge/srs]] — Full functional requirements (EARS)
- [[specs/073-orch-ensemble-merge/nfr]] — Quality targets (ISO/IEC 25010)
- [[specs/073-orch-ensemble-merge/plan]] — Step-by-step implementation plan
- [[specs/073-orch-ensemble-merge/tasks]] — Ordered developer task breakdown
- [[001-system-invariants/spec]] — Cross-cutting architectural invariants
- [[009-orchestration/spec]] — DAG planner, `DagScheduler`, `PlanVerifier`, parent spec for the orchestration subsystem this feature extends
- [[039-background-task-supervisor/spec]] — Binding async-supervision contract (NFR-AS-01..03)
- [[024-multi-model-design/spec]] — `*_provider` name-list reference pattern
- Paper: [ORCH (arXiv:2602.01797)](https://arxiv.org/abs/2602.01797)
