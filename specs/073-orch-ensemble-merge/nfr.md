---
aliases:
  - ORCH Ensemble-Merge NFR
  - Deterministic Verifier Ensemble NFR
  - NFR 5912
tags:
  - sdd
  - nfr
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/073-orch-ensemble-merge/brd]]"
  - "[[specs/073-orch-ensemble-merge/srs]]"
  - "[[specs/073-orch-ensemble-merge/spec]]"
  - "[[039-background-task-supervisor/spec]]"
---

# NFR: ORCH Deterministic Verifier Ensemble-Merge (GitHub #5912)

ISO/IEC 25010:2011 quality model.

---

## Performance Efficiency

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-PE-01 | Ensemble verify latency vs. single-provider verify | Bounded by the slowest responding member's timeout (`max(member_timeout_secs)` across N members), not `N × single-call latency` — `join_all` awaits all futures concurrently, not sequentially |
| NFR-PE-02 | Cost multiplier vs. single-agent baseline | Exactly `members.len()` × single-verify LLM cost per ensemble-treated decision; documented in config docs and the CLI/TUI stats surface (FR-016) so operators can reason about spend before opting in |
| NFR-PE-03 | Merge function CPU cost | O(N) over ballots — no allocation beyond `Vec<Gap>` union and a `HashMap`/small-vector tally; negligible versus the N LLM round-trips it aggregates |

---

## Determinism / Reproducibility

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-DR-01 | The pure `merge(ballots: &[Ballot]) -> MergeOutcome` function is deterministic | Same input `ballots` slice (any order) always produces the same `MergeOutcome` — unit-tested with fixed fixtures, zero LLM/network/randomness involvement (SRS FR-007) |
| NFR-DR-02 | End-to-end `verify()` with ensemble enabled is NOT claimed deterministic | Two invocations of the same ensemble on the same task output MAY yield different per-member ballots because LLM sampling is stochastic; this is documented explicitly, not silently assumed away (critic finding S3, resolved) |
| NFR-DR-03 | Member sampling temperature precondition | Ensemble members SHOULD be configured with `temperature = 0` to minimize sampling-induced ballot variance; stated as a documented precondition in config docs, not enforced in code (no per-member temperature override is added in PR-1) |
| NFR-DR-04 | `agreement_ratio` semantics are documented as agreement, not truth | Docs and code comments explicitly state that `agreement_ratio` measures ballot agreement among members, which can reflect sampling noise as well as genuine disagreement — it is never a calibrated correctness probability |

---

## Async Supervision (spec-039 Compliance)

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-AS-01 | Zero new `tokio::spawn()` call sites | The N-fold fan-out is `futures::future::join_all` over per-member futures, each independently `tokio::time::timeout`-wrapped, awaited inline on the current `SchedulerAction::Verify` handler execution — already running on a task supervised by the existing scheduler-loop invocation. Per `[[039-background-task-supervisor/spec]]`'s binding NEVER section (spawning with raw `tokio::spawn()` instead of a supervisor API), this is compliant because no new detached task is created — cancellation of the parent task transitively cancels all N in-flight futures |
| NFR-AS-02 | Await Discipline: no lock held across `.await` | The ensemble verifier holds no lock (`parking_lot` or otherwise) across any of the N concurrent `.await` points; provider handles are owned clones (`AnyProvider: Clone`), not lock-guarded references |
| NFR-AS-03 | Per-future timeout, not a single aggregate timeout | Each member's future is independently wrapped in `tokio::time::timeout(member_timeout_secs, ...)`, matching `PlanVerifier`'s existing per-call timeout pattern — a single slow member cannot block detection of the others' completion |
| NFR-AS-04 | Cost/visibility of inline-awaited futures | Because member calls are awaited inline (not spawned), they are invisible to the `bg_inflight` gauge; NFR-OB-01 (below) requires tracing spans as the compensating observability control |

---

## Reliability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-RE-01 | Never fail-closed | Below-quorum responses always fall back to the existing single-provider path, then to the existing fail-open `VerificationResult{complete:true, confidence:0.0}` — the ensemble path introduces zero new ways for `verify()` to block or error out task completion (SRS FR-008) |
| NFR-RE-02 | Errored/timed-out members never corrupt the merge | Excluded from both the vote tally and the confidence mean (SRS FR-003); verified by an explicit acceptance test (N=3, 1 error, merge over 2 responders) |
| NFR-RE-03 | No new panic path | Config validation failures (odd/≥3/no-duplicate) return `Result`/`ConfigError`, never panic; per-member timeout/error is `Result`-typed throughout the merge path |
| NFR-RE-04 | `should_replan` gate behavior is provably unchanged | Because `VerificationResult` gains no new field and `confidence` retains its original semantic (mean of self-reported member confidences), `scheduler_loop.rs:492-500` and `plan.rs:552-553` require zero code changes — verified by a test asserting the gate computation is byte-identical to the pre-feature version given equivalent inputs |

---

## Observability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-OB-01 | Tracing spans on ensemble member calls | Each ensemble member `chat_typed` call carries a `tracing::info_span!` following the `<crate>.<subsystem>.<operation>` convention (e.g. `orchestration.ensemble.verify_member`), so per-turn ensemble latency and count are visible in local Chrome JSON traces despite being invisible to `bg_inflight` (NFR-AS-04) |
| NFR-OB-02 | Per-member usage recording | Every ensemble member call records LLM token usage (SRS FR-016) — closes the existing zero-metering gap on the verify path |
| NFR-OB-03 | `ensemble_degraded` metric and warning | Fires exactly when quorum is not met and the fallback path is taken (SRS FR-009); never fires on a full-quorum ensemble round |
| NFR-OB-04 | CLI/TUI stats surface | Exposes per-member EMA score, observation count, most-recent `agreement_ratio`, and the `ensemble_degraded` counter (SRS FR-012), satisfying the TUI Rules mandatory status-indicator requirement while N-fold verification is in flight |

---

## Maintainability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-MA-01 | `EnsembleTracker` is a new, standalone type in `zeph-orchestration` | Not a generalization of `zeph-llm`'s `pub(crate)` RAPS `ReputationTracker` (Beta/Thompson-coupled, provider-scoped) and not an extension of `AdaptOrch`'s Thompson bandit (stochastic, topology-scoped) — the DRY trade-off (a second EMA-shaped tracker) is explicitly accepted per this project's pre-1.0 "extract to `zeph-common` only at the third consumer" rule |
| NFR-MA-02 | `merge()` is a free function or method with no hidden state | Testable with plain `Ballot` fixtures; no dependency on the tracker, provider handles, or config beyond the ballots themselves |
| NFR-MA-03 | All new `pub` items carry doc comments | Per CLAUDE.md's rustdoc requirements; `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-orchestration -p zeph-config -p zeph-core` passes clean |
| NFR-MA-04 | `OrchestrationError` gains at most one new `#[non_exhaustive]` variant if needed for a hard ensemble-config error | Existing LLM-step failures stay fail-open per NFR-RE-01; a hard error is reserved for load-time config validation only (SRS FR-014) |

---

## Compatibility / Scope Boundary

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-CO-01 | Default-off, zero behavior change | `enabled = false` (default) reproduces the exact current single-provider `PlanVerifier::verify()` code path — verified by a regression test (BRD SC-01) |
| NFR-CO-02 | `VerificationResult` type is unchanged | No new field added; `agreement_ratio` and per-member EMA live only on the internal `MergeOutcome`/`EnsembleTracker` types and the telemetry surface |
| NFR-CO-03 | No scheduler/subagent-spawn changes | PR-1 is scoped entirely to the existing inline-awaited `SchedulerAction::Verify` handler; `DagScheduler`, `SubAgentManager`, grants, and transcripts are untouched |
| NFR-CO-04 | Whole-plan verification untouched | `verify_plan()` / `replan_from_plan()` remain single-provider; this asymmetry is a deliberate, documented scope cut (SRS FR-D-06) |

---

## Usability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-US-01 | Config validation errors name the exact defect | "members.len() must be odd and >= 3, got N" / "duplicate provider name '<name>' in orchestration.ensemble.members" — never a generic validation failure |
| NFR-US-02 | Quorum-fallback is never silent | `ensemble_degraded` warning names the configured `members` count and the number that actually responded (NFR-OB-03) |
| NFR-US-03 | `--init` wizard clearly frames ensemble as opt-in and cost-multiplying | Prompt text states the `members.len()`× cost multiplier before enabling |
