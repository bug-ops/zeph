---
aliases:
  - ORCH Ensemble-Merge Plan
  - Deterministic Verifier Ensemble Plan
  - Plan 5912
tags:
  - sdd
  - plan
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/073-orch-ensemble-merge/spec]]"
  - "[[specs/073-orch-ensemble-merge/tasks]]"
---

# Implementation Plan: ORCH Deterministic Verifier Ensemble-Merge (GitHub #5912)

Source of truth: architect handoffs `.local/handoff/2026-07-13T19-47-29-architect.md` (base),
`.local/handoff/2026-07-13T19-54-56-architect.md` (S1/S2/S3+M1/M2 revision),
`.local/handoff/2026-07-13T20-00-06-architect.md` (S4/M5 revision, final), critic-approved
`.local/handoff/2026-07-13T20-02-30-critic.md` (verdict **minor / approved**). This plan sequences
the change set from `[[specs/073-orch-ensemble-merge/spec]]` §3 into an implementable order. No
architectural re-derivation — this is a formalization of the already-approved design.

**Decision Type:** `refactoring` (extends an existing subsystem; adds new in-crate module +
config + one wired consumer — no new crate). **Structure:** `workspace` (existing); new code in
`zeph-orchestration` + config in `zeph-config`; no cross-crate dependency added.

## Recommended Implementation Order

**Phase 1: `zeph-orchestration` — pure merge function + `EnsembleTracker`.** Implement first;
self-contained, no provider/network dependency, fully unit-testable in isolation. This is also
the phase that carries the NFR-DR-01 determinism proof.

**Phase 2: `zeph-config` — `EnsembleConfig` + load-time validation.** Implement second; depends on
nothing from Phase 1; independently unit-testable.

**Phase 3: Bootstrap wiring — resolve `members`, new `OrchestrationState` field.** Implement
third; depends on Phase 2's config type existing.

**Phase 4: `zeph-orchestration`/`zeph-core` — `EnsembleVerifier` wrapper + `SchedulerAction::Verify`
branch.** Implement fourth; depends on Phases 1-3 (merge function, config, resolved providers).
This is the only phase touching the hot scheduler-loop path.

**Phase 5: Observability — usage metering, `ensemble_degraded`, CLI/TUI stats.** Implement fifth;
depends on Phase 4's dispatch points existing to instrument.

**Phase 6: Mandatory integration points — `--init`, docs, playbook, coverage-status,
CHANGELOG.** Implement last; lowest risk, most mechanical.

---

## Phase 1: Pure Merge Function + `EnsembleTracker`

### P1-1: `Ballot` / `MergeOutcome` types and `merge()`

**File:** `crates/zeph-orchestration/src/ensemble/merge.rs` (new module)

1. `pub struct Ballot { pub member: String, pub complete: bool, pub confidence: f64, pub gaps: Vec<Gap> }` (reuses the existing `Gap` type from `verifier.rs`).
2. `pub struct MergeOutcome { pub complete: bool, pub gaps: Vec<Gap>, pub merged_confidence: f64, pub agreement_ratio: f64, pub tie_broken: bool }`.
3. `pub fn merge(ballots: &[Ballot]) -> MergeOutcome`:
   - Count `complete` votes; winner = majority (`votes_true > ballots.len()/2` vs. else).
   - On an exact tie (only reachable with an even `ballots.len()` after partial failures — see
     SRS FR-005): `complete = false` (fail-safe), `tie_broken = true`.
   - `gaps` = flattened union of `gaps` from ballots on the winning side (verbatim, no
     dedup/reconciliation — SRS FR-006/FR-D-07).
   - `merged_confidence` = arithmetic mean of `confidence` from ballots on the winning side.
   - `agreement_ratio` = `winning_side_count as f64 / ballots.len() as f64`.
4. No I/O, no async, no randomness — pure function of `ballots`. This is the NFR-DR-01 target.

### P1-2: `EnsembleTracker` (telemetry-only EMA)

**File:** `crates/zeph-orchestration/src/ensemble/tracker.rs` (new module)

1. `struct EmaEntry { score: f64, observations: u64 }`.
2. `pub struct EnsembleTracker { scores: HashMap<String, EmaEntry>, alpha: f64, decay: f64, min_observations: u32 }`.
3. `pub fn record(&mut self, member: &str, agreed: bool)` — `score ← α·(agreed as f64 in {0,1}) + (1−α)·score`; increments `observations`. Cold-start entries default to a neutral prior (e.g. `0.5`) until `observations >= min_observations`.
4. `pub fn ema(&self, member: &str) -> Option<f64>` — returns `None` below `min_observations` (cold-start gate), else the current score.
5. **No `select_subset` method** — this type exposes recording and reading only; it is never consulted to decide which members are dispatched (SRS FR-010/FR-011, critic S2).
6. Optional `save`/`load` following the RAPS pattern's *design* (atomic write, not shared file) — only if a persistence need is confirmed; otherwise in-memory-only for PR-1 is acceptable since the tracker is telemetry, not a correctness dependency.

### P1-3: Unit tests (Phase 1)

- `merge()`: unanimous complete, unanimous incomplete, 2-of-3 split (both directions), exact-tie fail-safe path, single-ballot edge case, empty-ballots edge case (documented as unreachable in practice — quorum ensures ≥1).
- S4 regression test: construct 3-of-3 unanimous incomplete+critical-gap ballots and 2-of-3 split ballots; assert `merged_confidence` is the mean of winning-side `confidence` inputs, NOT `agreement_ratio`, and manually recompute `should_replan` from the result to confirm it is not inverted relative to a hand-computed expectation.
- M6 regression test: 3 ballots constructed, then simulate 1 excluded (not passed to `merge()` at all) — confirm the 2-ballot merge's confidence mean does not include a `0.0` value.
- `EnsembleTracker`: cold-start gate (`ema()` returns `None` below `min_observations`), EMA update math, decay behavior.

**Phase 1 gate:** `cargo nextest run -p zeph-orchestration` green (ensemble module) before Phase 2.

---

## Phase 2: `EnsembleConfig` + Load-Time Validation

### P2-1: Config struct

**File:** `crates/zeph-config/src/experiment.rs` (sibling to `OrchestrationConfig`, new nested
`ensemble: EnsembleConfig` field with `#[serde(default)]`)

```rust
pub struct EnsembleConfig {
    pub enabled: bool,                 // default false
    pub verify: bool,                  // default false
    pub members: Vec<String>,          // default empty
    pub ema_alpha: f64,                // default 0.3
    pub ema_decay: f64,                // default 0.95
    pub min_observations: u32,         // default 5
    pub member_timeout_secs: u64,      // default 0 => fall back to verifier_timeout_secs
}
```

### P2-2: Load-time validation

**File:** `crates/zeph-config/src/loader.rs`, inside `validate_orchestration()`

1. `if self.orchestration.ensemble.enabled && self.orchestration.ensemble.verify { ... }` gate.
2. Odd/≥3 check: `members.len() % 2 == 1 && members.len() >= 3`, else
   `ConfigError::Validation("orchestration.ensemble.members must be odd and >= 3, got N")`.
3. Duplicate check: build a `HashSet<&str>` from `members`; if `set.len() != members.len()`,
   `ConfigError::Validation("orchestration.ensemble.members contains a duplicate provider name")`.
4. Both checks placed immediately after the existing `completeness_threshold` check for locality.

### P2-3: Unit tests (Phase 2)

- Default config: `ensemble.enabled == false`, validation passes trivially (checks skipped).
- `enabled=true, verify=true`, even-length `members` → `Err`.
- `enabled=true, verify=true`, 1-element `members` → `Err`.
- `enabled=true, verify=true`, duplicate names → `Err`.
- `enabled=true, verify=true`, valid odd/≥3/unique `members` → `Ok`.
- `enabled=true, verify=false` with an even/invalid `members` list → `Ok` (checks skipped per SRS FR-014's "not applied" clause) — confirms an unused/staged config doesn't block startup.
- TOML round-trip serde test (mirrors `completeness_threshold_serde_round_trip` in `experiment.rs`).

**Phase 2 gate:** `cargo nextest run -p zeph-config` green before Phase 3.

---

## Phase 3: Bootstrap Wiring

### P3-1: `OrchestrationState.ensemble_members` field

**File:** `crates/zeph-core/src/agent/state/mod.rs`

Add `pub(crate) ensemble_members: Vec<AnyProvider>` to `OrchestrationState`, sibling to
`verify_provider` (`:653`). Defaults to empty `Vec` (matches `#[derive(Default)]` on
`OrchestrationState`).

### P3-2: `Bootstrap::build_ensemble_members()`

**File:** `src/bootstrap/mod.rs`, sibling to `build_verify_provider()` (`:1916`)

```rust
pub fn build_ensemble_members(&self) -> Vec<AnyProvider> {
    if !self.config.orchestration.ensemble.enabled { return Vec::new(); }
    self.config.orchestration.ensemble.members.iter()
        .filter_map(|name| match create_named_provider(name, &self.config) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(provider = %name, error = %e,
                    "ensemble member resolution failed — excluded from ensemble");
                None
            }
        })
        .collect()
}
```

Mirrors `build_verify_provider`'s log-and-continue-on-failure pattern. A resolution failure here
shrinks the effective ensemble at startup (still subject to the P4 runtime quorum check, not a
hard failure) — document this explicitly as an accepted PR-1 behavior distinct from the
runtime-level FR-014 config validation (which catches shape defects, not resolution failures).

### P3-3: Builder wiring

Thread `build_ensemble_members()`'s result into `OrchestrationState.ensemble_members` via the
existing services-construction call site (same call site that sets `verify_provider` from
`build_verify_provider()`).

### P3-4: Unit tests (Phase 3)

- `build_ensemble_members()` with `enabled=false` → empty `Vec`.
- `build_ensemble_members()` with all-valid names → `Vec` of length `members.len()`.
- `build_ensemble_members()` with one unresolvable name → `Vec` of length `members.len() - 1`,
  warning logged.

**Phase 3 gate:** `cargo nextest run -p zeph-core --lib` (bootstrap/state tests) green before
Phase 4.

---

## Phase 4: `EnsembleVerifier` Wrapper + Scheduler-Loop Branch

### P4-1: `EnsembleVerifier`

**File:** `crates/zeph-orchestration/src/ensemble/verifier.rs` (new module)

```rust
pub struct EnsembleVerifier {
    members: Vec<AnyProvider>,
    member_timeout: Duration,
    tracker: EnsembleTracker,
}

impl EnsembleVerifier {
    pub async fn verify(&mut self, task: &TaskNode, output: &str, sanitizer: &dyn OutputSanitizer)
        -> VerificationResult
    {
        let quorum = self.members.len() / 2 + 1;
        let futures = self.members.iter().map(|m| {
            tokio::time::timeout(self.member_timeout, m.chat_typed::<VerificationResult>(&build_messages(task, output, sanitizer)))
        });
        let results = futures::future::join_all(futures).await;
        let ballots: Vec<Ballot> = results.into_iter()
            .filter_map(|r| r.ok().and_then(Result::ok))   // timeout Err + LLM Err both excluded (FR-003)
            .map(|vr| Ballot { member: /* track which member produced vr */, complete: vr.complete, confidence: vr.confidence, gaps: vr.gaps })
            .collect();

        if ballots.len() < quorum {
            // FR-008: caller falls back to single-provider verify_provider path.
            return QuorumNotMet; // exact fallback control-flow shape is an implementation detail
        }

        let outcome = merge(&ballots);
        for ballot in &ballots {
            self.tracker.record(&ballot.member, ballot.complete == outcome.complete);
        }
        // Record agreement_ratio/tracker state to telemetry (Phase 5), NOT into the returned value.
        VerificationResult { complete: outcome.complete, gaps: outcome.gaps, confidence: outcome.merged_confidence }
    }
}
```

Note: associating each `Result` back to its originating member name (for `Ballot.member` and
`EnsembleTracker.record`) requires zipping `self.members`/config names with the `join_all` output
in dispatch order — `join_all` preserves input order, so this is a straightforward `zip`, not a
race.

### P4-2: Quorum fallback control flow

The exact `QuorumNotMet` signal shape (enum variant, `Option<VerificationResult>`, or a `Result`)
is an implementation detail left to the developer — the requirement (SRS FR-008) is: when quorum
is not met, control returns to the `SchedulerAction::Verify` handler, which then calls the
existing single-provider `PlanVerifier::verify()` exactly as it does today (including that
function's own existing fail-open path), and increments `ensemble_degraded` (Phase 5) before
doing so.

### P4-3: `SchedulerAction::Verify` handler branch

**File:** `crates/zeph-core/src/agent/scheduler_loop.rs:459-535`

Add a branch at the top of the existing `SchedulerAction::Verify { task_id, output } => { ... }`
arm:

```rust
let result = if self.services.orchestration.orchestration_config.ensemble.enabled
    && self.services.orchestration.orchestration_config.ensemble.verify
    && !self.services.orchestration.ensemble_members.is_empty()
{
    match ensemble_verifier.verify(&task, &output, &sanitizer).await {
        QuorumMet(vr) => vr,
        QuorumNotMet => {
            ensemble_degraded_counter.increment();
            tracing::warn!(task_id = %task_id, "ensemble quorum not met — falling back to single-provider verify");
            verifier.verify(&task, &output).await   // existing single-call path, unchanged
        }
    }
} else {
    verifier.verify(&task, &output).await            // existing single-call path, unchanged
};
```

Everything downstream of `result` (the `should_replan` computation, `replan()` call,
`inject_tasks()`) is **completely unmodified** — this is the concrete implementation of the "zero
code changes downstream" claim in the spec (S4).

### P4-4: Unit/integration tests (Phase 4)

- Full end-to-end test with a mock/stub provider set: ensemble enabled, 3 members, all agree →
  merged result matches expectation, `should_replan` computed correctly.
- Quorum-not-met path: 2-of-3 members stubbed to error → single-provider fallback invoked,
  `ensemble_degraded` incremented.
- Disabled path: `enabled=false` → `ensemble_verifier` never constructed/called, identical to
  pre-feature behavior (regression test, BRD SC-01).

**Phase 4 gate:** `cargo nextest run -p zeph-orchestration -p zeph-core` green before Phase 5.

---

## Phase 5: Observability

### P5-1: Per-member usage recording

**File:** `crates/zeph-orchestration/src/ensemble/verifier.rs`

Wrap each member's `chat_typed` call to record token usage via the existing usage-recording
mechanism used elsewhere in the LLM call path (mirror whatever `CostTracker`/usage-recording hook
other `chat_typed` call sites use — `PlanVerifier::verify()` currently does NOT do this, so this
is net-new instrumentation, not a copy of an existing verifier.rs pattern).

### P5-2: `ensemble_degraded` metric

Add a counter (Prometheus-gated where applicable, matching the project's existing metrics
pattern) incremented exactly at the P4-3 quorum-fallback branch. Emit the `warn!` at the same
site (already shown in P4-3).

### P5-3: Tracing spans

Add `tracing::info_span!("orchestration.ensemble.verify_member", member = %name)` around each
member's `chat_typed` call (NFR-OB-01), and a parent span
`tracing::info_span!("orchestration.ensemble.verify")` around the whole `EnsembleVerifier::verify`
call.

### P5-4: CLI/TUI stats surface

Expose: per-member EMA score + observation count (from `EnsembleTracker`), most-recent
`agreement_ratio`, and the `ensemble_degraded` counter value. Follow the existing pattern used for
other orchestration stats surfaces (e.g. `AdaptOrch`'s bandit-state display, if one exists, or the
existing plan-status TUI view) — exact widget/command placement is a developer decision within
the existing TUI/CLI stats infrastructure.

### P5-5: Unit tests (Phase 5)

- Usage recording: confirm N usage records are produced per ensemble decision (not zero).
- `ensemble_degraded` counter increments exactly once per quorum-fallback event, never on a
  full-quorum round.

**Phase 5 gate:** `cargo nextest run` green across touched crates before Phase 6.

---

## Phase 6: Mandatory Integration Points

| # | Point | Where |
|---|-------|-------|
| 1 | `config.toml` section | `[orchestration.ensemble]` — documented in `docs/src/` |
| 2 | CLI subcommand/argument | N/A for PR-1 (no new CLI flag needed beyond config; ensemble stats surfaced via existing stats commands, P5-4) |
| 3 | TUI command palette / `/` command | Ensemble stats surface (P5-4); no spinner needed beyond the existing per-task verify status indicator, extended to note ensemble mode is active |
| 4 | `--init` wizard | New prompt: "Enable ensemble-verified plan verification? (opt-in, multiplies verify cost by member count)" → if yes, prompt for `members` list (validated against P2-2's odd/≥3/no-duplicate rule at wizard time, not just at load) |
| 5 | `--migrate-config` | N/A — new optional table with all-default values that preserve current behavior; document this explicitly (SRS FR-017 item 3) rather than silently omitting a migration step |
| 6 | Testing playbook | Create `/Users/rabax/Dev/zeph/.local/testing/playbooks/orch-ensemble-merge.md` (main-repo path) — scenarios: default-off regression, full-quorum merge, quorum-fallback, config validation rejections, S4/M6 regression scenarios |
| 7 | Coverage status | Add rows in `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` for the ensemble verifier, config validation, and CLI/TUI stats surface (status `Untested`) |

### P6-1: CHANGELOG.md

Add an `[Unreleased]` entry describing the opt-in ensemble-verified plan verification feature.

---

## Pre-Merge Checklist

- [ ] `cargo +nightly fmt --check`
- [ ] `cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`
- [ ] `cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler" --lib --bins`
- [ ] `RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] `cargo test --doc --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] Async-supervision scan (`.claude/rules/continuous-improvement.md`) confirms zero new `tokio::spawn()` sites
- [ ] `CHANGELOG.md` updated (`[Unreleased]`)
- [ ] `.local/testing/playbooks/orch-ensemble-merge.md` created (main-repo path)
- [ ] `.local/testing/coverage-status.md` rows added (main-repo path)
- [ ] LLM serialization gate: N/A unless `chat_typed`'s request/response serialization itself is
      touched — if only the dispatch/merge logic around existing `chat_typed` calls changes, this
      gate does not apply; confirm and record in the PR description
- [ ] `specs/README.md` and `specs/MOC-specs.md` register `073-orch-ensemble-merge`
