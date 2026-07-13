---
aliases:
  - ORCH Ensemble-Merge BRD
  - Deterministic Verifier Ensemble BRD
  - BRD 5912
tags:
  - sdd
  - brd
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/073-orch-ensemble-merge/spec]]"
  - "[[specs/073-orch-ensemble-merge/srs]]"
  - "[[specs/073-orch-ensemble-merge/nfr]]"
  - "[[009-orchestration/spec]]"
---

# BRD: ORCH Deterministic Verifier Ensemble-Merge (GitHub #5912)

## 1. Business Context

The paper "ORCH: many analyses, one merge — a deterministic multi-agent orchestrator for
discrete-choice reasoning with EMA-guided routing" (arXiv:2602.01797, Frontiers in AI,
2026-02-02, Zhou & Chan) proposes a training-free pattern for accuracy-critical discrete-choice
decisions: N independently-selected agents produce structured judgments in parallel, and a
deterministic (non-LLM) merge rule reconciles them into one final answer. A prior research spec
(`.local/specs/054-orch-ensemble-merge-discrete-choice/spec.md`, GitHub #5912) audited Zeph's
orchestration subsystem and confirmed no existing primitive (`AgentRouter`, `AdaptOrch`,
`PlanVerifier`, `Aggregator`) implements this "redundant-parallel-execution + deterministic-merge"
pattern — every existing router selects exactly one path per task.

`PlanVerifier::verify()` (`crates/zeph-orchestration/src/verifier.rs:159`) is Zeph's highest-stakes
discrete-choice decision point already in production: it issues a single LLM call to classify
whether a completed task's output is acceptable or has gaps, and that verdict gates an expensive
`replan()` cycle. A single noisy LLM call currently has no redundancy check before triggering (or
suppressing) a replan.

## 2. Problem Statement

Zeph has no mechanism to raise confidence in a single high-stakes discrete-choice verdict by
consulting multiple independent LLM judgments and combining them with a fixed, reproducible rule.
Every verify decision today rests on exactly one `chat_typed::<VerificationResult>` call
(`verify_provider`, fail-open on error/timeout). A transient misjudgment by that one provider is
indistinguishable from a genuine gap, and there is no way to trade a bounded, opt-in cost increase
for higher confidence on this specific decision.

## 3. Business Goals

| ID | Goal | Priority |
|----|------|----------|
| BG-01 | An operator can opt a Zeph deployment into ensemble-verified plan verification: N configured provider members independently classify the same completed-task output, and a deterministic majority rule produces the single verdict `replan()` already consumes | P1 |
| BG-02 | The ensemble path degrades gracefully to today's exact single-provider behavior whenever it is not explicitly enabled, whenever quorum is not met, or whenever the ensemble path is misconfigured — never a new failure mode, never fail-closed | P1 |
| BG-03 | Per-member LLM cost of the N-fold fan-out is observable (token usage) and quorum degradation is surfaced (metric + warning), closing an existing observability gap where verify calls are completely unmetered today | P2 |
| BG-04 | The capability is scoped as a minimal, reviewable PR-1: no scheduler/subagent-spawn changes, no new `tokio::spawn` call site, no change to the downstream `should_replan` gate semantics | P1 |

## 4. Stakeholders

| Role | Interest |
|------|----------|
| Operator running accuracy-critical orchestration plans | Wants higher-confidence replan-vs-accept decisions at a bounded, opt-in cost |
| Zeph maintainers | Close a documented research gap (#5912) with a minimal, spec-039-compliant PR-1; keep the door open to later phases (subagent fan-out, sanitizer/tools consumers) without committing to them now |
| Future `/sdd plan` sessions (phase-2+) | Inherit a clean `EnsembleTracker`/merge-function boundary that can be reused or extended for full-node subagent fan-out |

## 5. Out of Scope

| Item | Reason |
|------|--------|
| EMA-gated subset selection (`select_subset(k)`) | Rewarding agreement-with-majority and then using that reward to gate which members vote next round creates a self-reinforcing consensus-collapse loop that erodes ensemble diversity (critic finding S2) — deferred until a ground-truth (not self-consistency) reward signal exists |
| Ordinal/severity-class merge (4-way `GapSeverity` vote) | A plurality vote over 4 classes ties far more often than a binary vote and makes the severe-tie-break load-bearing for the common case, defeating denoising by collapsing to the single most-pessimistic member (critic finding S1) — deferred as a documented future phase |
| Weighted majority vote (EMA-weighted ballots) | EMA already governs cost/telemetry; using it to weight votes too would double-count reputation and muddy reproducibility — documented future option |
| Ground-truth (delayed) reward signal for the EMA tracker | No current mechanism observes whether a verify verdict was actually correct after the fact; v1's EMA reward is a self-consistency proxy, recorded for telemetry only, never gating participation |
| Phase-2 full-DAG-node subagent fan-out (`SchedulerAction::Spawn` × N for one `TaskNode`) | A materially larger change (grants, transcripts, scheduler wiring); PR-1 deliberately proves the ensemble/merge/tracker primitives on the smaller verify-only seam first |
| `zeph-sanitizer` / `zeph-tools` as ensemble consumers (injection-risk classification, tool-call arbitration) | Both live in crates that do not depend on `zeph-orchestration`; adopting them first would force moving the ensemble core to a shared crate — deferred to a future phase per the original research spec's Ask-First item |
| Whole-plan verification (`verify_plan()` / `replan_from_plan()`) | Stays single-provider in v1; only the per-task `Verify` action is ensemble-treated — a deliberate, documented scope cut, not an oversight |
| Reproducing the paper's evaluation methodology/benchmarks in `zeph-bench` | Out of scope for this implementation spec |

These deferrals are explicit and carried into `srs.md` as acknowledged-deferred requirements.

## 6. Success Criteria

| ID | Criterion | Measurable |
|----|-----------|-----------|
| SC-01 | With `[orchestration.ensemble] enabled = false` (default), any DAG plan runs through the exact current single-provider `PlanVerifier::verify()` path — byte-for-byte unchanged behavior | Regression test: default config produces identical `VerificationResult` code path as pre-feature baseline |
| SC-02 | With `enabled = true, verify = true` and an odd, ≥3, duplicate-free `members` list, a `Verify` action dispatches N parallel `chat_typed::<VerificationResult>` calls and merges them via a deterministic binary-majority vote on `complete` | Unit test: fixed ballots in, deterministic `MergeOutcome` out, no LLM involved |
| SC-03 | The merged `VerificationResult.confidence` is the mean of the winning-side members' own self-reported `confidence` values — the existing `should_replan` gate in `scheduler_loop.rs`/`plan.rs` requires zero code changes | Test: construct a 3-of-3 unanimous incomplete+critical-gap ballot set and a 2-of-3 split set; confirm gate behavior matches hand-computed expectation, not inverted |
| SC-04 | A member that errors or times out is excluded from the ballot entirely — not counted as a vote, not dragging the confidence mean toward 0.0 | Test: N=3, 1 member errors, merge proceeds over the 2 responders; confidence mean excludes the errored member (critic finding M6) |
| SC-05 | Below-quorum responses (timeouts/errors) fall back to the existing single-provider `verify_provider` path, and ultimately to the existing fail-open `VerificationResult{complete:true, confidence:0.0}` if that also fails — never a new failure mode | Test: quorum not met → verify single-provider fallback fires; `ensemble_degraded` metric/warn observed |
| SC-06 | `members.len()` must be odd and ≥ 3, and duplicate provider names are rejected, at config load time — never at merge time | Config validation test: even-length, short, and duplicate-name `members` lists all fail `--validate`/startup with `ConfigError::Validation` |
| SC-07 | Per-member LLM token usage is recorded for every ensemble verify call | Metrics/telemetry inspection shows N usage records per ensemble decision, not zero as today |
| SC-08 | Zero new `tokio::spawn()` call sites are introduced; the N-fold fan-out is inline `futures::future::join_all` awaited on the existing supervised scheduler-loop task | `.claude/rules/continuous-improvement.md` async-supervision scan count is non-increasing after this PR |

## 7. Constraints

- No new crate; all new code lands in `zeph-orchestration` (tracker, merge function, ensemble
  verifier wrapper) plus config fields in `zeph-config` and bootstrap wiring in the root binary
  crate (`src/bootstrap/`) and `crates/zeph-core/src/agent/state/mod.rs`'s `OrchestrationState`.
- Zero new `tokio::spawn()` call sites (spec-039 binding constraint, see
  `[[039-background-task-supervisor/spec]]`).
- No new lock or synchronization primitive on the scheduler-loop hot path.
- Default `enabled = false` — zero behavior change unless explicitly opted in (BG-02).
- `size`/`min_quorum` are derived, not independently configurable — removes two operator
  footguns and makes the no-tie guarantee structural (critic finding M5, resolved).
- `VerificationResult` gains no new field — `agreement_ratio` lives in an internal
  `MergeOutcome` type and telemetry only, never in the type `replan()` consumes (critic finding
  S4, resolved).

## 8. Dependencies

| Dependency | Type | Notes |
|------------|------|-------|
| `PlanVerifier::verify()` (`crates/zeph-orchestration/src/verifier.rs:159`) | Internal | Existing single-call verify path; ensemble wraps/extends it, does not replace its fail-open contract |
| `SchedulerAction::Verify { task_id, output }` handling (`crates/zeph-core/src/agent/scheduler_loop.rs:459-535`) | Internal | Existing inline-awaited, already-supervised dispatch seam the ensemble slots into |
| `create_named_provider(name, config) -> AnyProvider` (`src/bootstrap/provider.rs:279`) | Internal | Existing provider-name resolution seam; ensemble `members` resolve through it, same as `build_verify_provider` (`src/bootstrap/mod.rs:1916`) |
| `OrchestrationState.verify_provider: Option<AnyProvider>` (`crates/zeph-core/src/agent/state/mod.rs:653`) | Internal | Sibling field pattern the new `ensemble_members: Vec<AnyProvider>` field mirrors |
| `completeness_threshold` validation (`crates/zeph-config/src/loader.rs::validate_orchestration`) | Internal | Existing validation function the new odd/≥3/no-duplicate `members` check is added alongside |
| `zeph_common::task_supervisor` / `BackgroundSupervisor` | Internal | Not used directly by PR-1 (inline `join_all` on the already-supervised scheduler-loop task is spec-039 compliant without a new spawn site) — cited to confirm no violation |
