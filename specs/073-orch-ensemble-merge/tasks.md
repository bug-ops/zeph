---
aliases:
  - ORCH Ensemble-Merge Tasks
  - Deterministic Verifier Ensemble Tasks
  - Tasks 5912
tags:
  - sdd
  - tasks
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/073-orch-ensemble-merge/plan]]"
  - "[[specs/073-orch-ensemble-merge/spec]]"
---

# Task Breakdown: ORCH Deterministic Verifier Ensemble-Merge (GitHub #5912)

All tasks reference `[[specs/073-orch-ensemble-merge/plan]]`. This is the developer's primary
implementation checklist alongside the architect/critic handoffs referenced there. Implement in
phase order — each phase depends only on prior phases. This document itself is a design artifact;
per this spec package's scope, no code is implemented as part of producing it (see spec.md §Out
of Scope) — implementation is picked up by a future `new-feature` team-develop session.

---

## Phase 1: Pure Merge Function + `EnsembleTracker`

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T1.1 | Add `Ballot` struct | P1-1 | `crates/zeph-orchestration/src/ensemble/merge.rs` (new) | `member`, `complete`, `confidence`, `gaps` |
| T1.2 | Add `MergeOutcome` struct | P1-1 | same | `complete`, `gaps`, `merged_confidence`, `agreement_ratio`, `tie_broken` — internal only, never surfaces in `VerificationResult` |
| T1.3 | Implement `merge(ballots: &[Ballot]) -> MergeOutcome` | P1-1 | same | Binary majority on `complete`; winning-side gap union; confidence = mean of winning-side `confidence`; exact-tie fail-safe = `complete:false` |
| T1.4 | Add `EmaEntry` + `EnsembleTracker` struct | P1-2 | `crates/zeph-orchestration/src/ensemble/tracker.rs` (new) | `scores: HashMap<String, EmaEntry>`, `alpha`, `decay`, `min_observations` |
| T1.5 | Implement `record(member, agreed: bool)` and `ema(member) -> Option<f64>` | P1-2 | same | Cold-start gate: `ema()` returns `None` below `min_observations`; **no `select_subset` method** (S2) |
| T1.6 | Unit tests: `merge()` — unanimous, 2-of-3 split (both directions), exact tie, single-ballot, empty-ballots | P1-3 | `merge.rs` | Required coverage, not optional |
| T1.7 | Unit test: S4 regression — 3-of-3 unanimous incomplete+critical vs. 2-of-3 split; confirm `merged_confidence` ≠ `agreement_ratio` and hand-computed `should_replan` is not inverted | P1-3 | `merge.rs` | Blocking acceptance criterion (BRD SC-03) |
| T1.8 | Unit test: M6 regression — construct 3 ballots, merge only 2 (simulating 1 excluded), confirm confidence mean excludes the missing ballot, not a `0.0` | P1-3 | `merge.rs` | Blocking acceptance criterion (BRD SC-04) |
| T1.9 | Unit tests: `EnsembleTracker` cold-start gate + EMA update math + decay | P1-3 | `tracker.rs` | |

**Phase 1 gate:** `cargo nextest run -p zeph-orchestration` green (ensemble module) before Phase 2.

---

## Phase 2: `EnsembleConfig` + Load-Time Validation

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T2.1 | Add `EnsembleConfig` struct with `#[serde(default)]` fields | P2-1 | `crates/zeph-config/src/experiment.rs` | `enabled`, `verify` (both default `false`), `members` (default empty), `ema_alpha=0.3`, `ema_decay=0.95`, `min_observations=5`, `member_timeout_secs=0` |
| T2.2 | Add `ensemble: EnsembleConfig` field to `OrchestrationConfig` | P2-1 | same | `#[serde(default)]` — no breaking change to existing configs |
| T2.3 | Add odd/≥3 validation in `validate_orchestration()` | P2-2 | `crates/zeph-config/src/loader.rs` | Gated on `enabled && verify`; placed next to the existing `completeness_threshold` check |
| T2.4 | Add no-duplicate-member-name validation | P2-2 | same | Same gate; `ConfigError::Validation` with the offending name in the message |
| T2.5 | Unit tests: default config passes trivially; even-length/short/duplicate `members` rejected when `enabled&&verify`; valid config accepted; `enabled=true,verify=false` with an invalid list still passes (checks skipped) | P2-3 | `loader.rs`, `experiment.rs` | Full matrix required — this is the M5/M7 acceptance surface |
| T2.6 | TOML serde round-trip test for `EnsembleConfig` | P2-3 | `experiment.rs` | Mirrors existing `completeness_threshold_serde_round_trip` pattern |

**Phase 2 gate:** `cargo nextest run -p zeph-config` green before Phase 3.

---

## Phase 3: Bootstrap Wiring

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T3.1 | Add `ensemble_members: Vec<AnyProvider>` field to `OrchestrationState` | P3-1 | `crates/zeph-core/src/agent/state/mod.rs` | Sibling to `verify_provider` (`:653`); defaults empty via `#[derive(Default)]` |
| T3.2 | Implement `Bootstrap::build_ensemble_members() -> Vec<AnyProvider>` | P3-2 | `src/bootstrap/mod.rs` | Mirrors `build_verify_provider` (`:1916`); log-and-skip on individual resolution failure via `create_named_provider` |
| T3.3 | Wire `build_ensemble_members()` result into `OrchestrationState.ensemble_members` at the services-construction call site | P3-3 | wherever `verify_provider` is currently set from `build_verify_provider()` | Same call site/pattern |
| T3.4 | Unit tests: `enabled=false` → empty `Vec`; all-valid names → full `Vec`; one unresolvable name → shrunk `Vec` + warning logged | P3-4 | `src/bootstrap/mod.rs` | |

**Phase 3 gate:** `cargo nextest run -p zeph-core --lib` green (bootstrap/state tests) before Phase 4.

---

## Phase 4: `EnsembleVerifier` + Scheduler-Loop Branch

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T4.1 | New `EnsembleVerifier` struct (`members`, `member_timeout`, `tracker`) | P4-1 | `crates/zeph-orchestration/src/ensemble/verifier.rs` (new) | |
| T4.2 | Implement `verify()`: `join_all` over per-member `timeout(chat_typed::<VerificationResult>)`, exclude `Err`/timeout ballots (FR-003), quorum check (`members.len()/2+1`), call `merge()`, record each ballot's agreement into `tracker` | P4-1 | same | Zip `join_all`'s ordered output back to member names for `Ballot.member`/`tracker.record` — `join_all` preserves input order |
| T4.3 | Define the quorum-not-met signal shape (enum/`Option`/`Result` — developer's choice) so the caller can fall back | P4-2 | same | Must NOT reuse `PlanVerifier::fail_open()` per-member (M6); fail-open, if any, applies only once at the ensemble level when the caller falls through to the existing single-provider path |
| T4.4 | Add ensemble branch at the top of `SchedulerAction::Verify` handler | P4-3 | `crates/zeph-core/src/agent/scheduler_loop.rs:459-535` | `enabled && verify && !ensemble_members.is_empty()` → `EnsembleVerifier::verify`; else/on quorum-not-met → existing single-call `verifier.verify()`, completely unmodified downstream (`should_replan`, `replan()`, `inject_tasks()`) |
| T4.5 | Integration test: 3 stub members all agree → merged result correct, `should_replan` computed correctly | P4-4 | `scheduler_loop.rs` tests or `zeph-orchestration` integration tests | |
| T4.6 | Integration test: 2-of-3 stub members error → single-provider fallback invoked | P4-4 | same | |
| T4.7 | Regression test: `enabled=false` → `EnsembleVerifier` never constructed/called, behavior identical to pre-feature baseline (BRD SC-01) | P4-4 | same | Blocking |

**Phase 4 gate:** `cargo nextest run -p zeph-orchestration -p zeph-core` green before Phase 5.

---

## Phase 5: Observability

| # | Task | Plan Step | File | Notes |
|---|------|-----------|------|-------|
| T5.1 | Record per-member LLM token usage on each ensemble `chat_typed` call | P5-1 | `crates/zeph-orchestration/src/ensemble/verifier.rs` | Net-new instrumentation — `PlanVerifier::verify()` does not do this today |
| T5.2 | Add `ensemble_degraded` counter, incremented exactly at the quorum-fallback branch | P5-2 | `scheduler_loop.rs` (T4.4 site) | Prometheus-gated where applicable, matching existing metrics conventions |
| T5.3 | Add `warn!` log at the same quorum-fallback site, naming configured vs. responding member counts | P5-2 | same | |
| T5.4 | Add `tracing::info_span!("orchestration.ensemble.verify_member", ...)` per member call + parent `orchestration.ensemble.verify` span | P5-3 | `verifier.rs` | NFR-OB-01; `<crate>.<subsystem>.<operation>` naming convention |
| T5.5 | CLI/TUI stats surface: per-member EMA + observation count, latest `agreement_ratio`, `ensemble_degraded` counter | P5-4 | existing stats/TUI infrastructure | Exact widget/command placement is a developer decision |
| T5.6 | Unit tests: usage recording produces N records per ensemble decision; `ensemble_degraded` increments exactly on fallback, never on full-quorum rounds | P5-5 | touched files | |

**Phase 5 gate:** `cargo nextest run` green across touched crates before Phase 6.

---

## Phase 6: Mandatory Integration Points and Documentation

| # | Task | Integration Point | Path | Notes |
|---|------|--------------------|------|-------|
| T6.1 | Document `[orchestration.ensemble]` config section | #1 config.toml | `docs/src/` | |
| T6.2 | `--init` wizard prompt: opt-in + member list, cost-multiplier warning, odd/≥3/no-duplicate validated at wizard time | #4 `--init` | `src/init/mod.rs` | |
| T6.3 | Confirm no `--migrate-config` step is needed; record rationale (new optional all-default table) | #5 migrate-config | PR description | N/A task, documented not silent |
| T6.4 | Create testing playbook | #6 | `/Users/rabax/Dev/zeph/.local/testing/playbooks/orch-ensemble-merge.md` | Main-repo path (not worktree); scenarios: default-off regression, full-quorum merge, quorum-fallback, config-validation rejections, S4/M6 regressions |
| T6.5 | Add coverage-status rows | #7 | `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` | Rows for ensemble verifier, config validation, CLI/TUI stats surface — status `Untested` |
| T6.6 | Update `CHANGELOG.md` `[Unreleased]` | — | `CHANGELOG.md` | Root |
| T6.7 | Register spec in `specs/README.md` and `specs/MOC-specs.md` | — | `specs/` | Already done by the `sdd` agent as part of this spec package — verify present |

**#2 (CLI subcommand) and #3 (TUI palette) are covered by T5.5 — ensemble is config-driven with a
stats surface, no new imperative CLI subcommand needed for PR-1.**

---

## Acceptance Criteria (for PR merge)

- [ ] All Phase 1-5 unit/integration tests pass: `cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler" --lib --bins`
- [ ] `cargo +nightly fmt --check`
- [ ] `cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`
- [ ] Rustdoc gate: `RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] Doc-tests: `cargo test --doc --workspace --features "desktop,ide,server,chat,pdf,scheduler"`
- [ ] S4 regression test present and passing (T1.7)
- [ ] M6 regression test present and passing (T1.8)
- [ ] M7 config-validation test present and passing (T2.5, duplicate-name case)
- [ ] Default-off regression test present and passing (T4.7)
- [ ] Async-supervision scan shows zero new `tokio::spawn()` sites introduced by this PR
- [ ] `CHANGELOG.md` updated
- [ ] Testing playbook + coverage-status rows added (main-repo `.local/testing/` path)
- [ ] `specs/README.md` and `specs/MOC-specs.md` register `073-orch-ensemble-merge`
