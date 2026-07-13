---
aliases:
  - Orchestration Node Control Parity NFR
  - Node Timeout / Retry-Exhausted Recovery NFR
  - NFR 6021
tags:
  - sdd
  - nfr
  - orchestration
created: 2026-07-13
status: approved
related:
  - "[[specs/075-orchestration-node-control-parity/brd]]"
  - "[[specs/075-orchestration-node-control-parity/srs]]"
  - "[[specs/075-orchestration-node-control-parity/spec]]"
  - "[[039-background-task-supervisor/spec]]"
---

# NFR: Orchestration Node Control Parity — Per-Task Timeouts and Retry-Exhausted Recovery (GitHub #6021)

ISO/IEC 25010:2011 quality model.

---

## Performance Efficiency

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-PE-01 | `check_timeouts()` per-task effective-timeout lookup adds no complexity class | Remains `O(self.running)` — the per-task override lookup is an `Option`/`map`/`unwrap_or` chain on data already held by the loop, not a graph traversal (SRS FR-002) |
| NFR-PE-02 | `wait_event()` nearest-deadline computation adds no complexity class | Remains `O(self.running)` — per-task effective timeout replaces the single global value inside the existing `.map(...).min()` chain, no additional pass over `self.graph.tasks` (SRS FR-003) |
| NFR-PE-03 | Recovery mutation cost | `O(1)` — a status flip and a `TaskResult` construction inside `propagate_failure()`, no additional graph traversal beyond what the function already performs (SRS FR-007) |
| NFR-PE-04 | `validate()` guard cost | `O(1)` per task, added to the existing per-task loop (`dag.rs:51-77`) — no new pass over `tasks` (SRS FR-011, FR-012) |

---

## Reliability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-RE-01 | Zero regression for graphs using neither feature | A graph where every `TaskNode.timeout` and `TaskNode.recovery` is `None` produces byte-identical scheduling/failure behavior to pre-feature code — verified by a regression test covering `check_timeouts()`, `wait_event()`, and `propagate_failure()` (BRD SC-01) |
| NFR-RE-02 | `Skip`/`Ask` semantics are unchanged | Recovery is additive and scoped to the `Abort`-default/retry-exhausted-`Retry` branches only; a node with `recovery` configured under `Skip`/`Ask` is inert (warned, not enforced) — the `Skip`/`Ask` code paths themselves are untouched (SRS FR-012) |
| NFR-RE-03 | Cascade-abort precedence never regresses to recovery-first | Recovery is reachable only through `propagate_failure()`, which the existing cascade-check `return`s bypass entirely on a cascade trip — no new code path allows recovery to preempt a cascade abort (SRS FR-013) |
| NFR-RE-04 | No new panic path | `TimeoutPolicy`/`RecoveryAction` construction and the `validate()` guards are `Option`/`Result`-typed throughout; no `unwrap()`/`expect()` introduced on a value that can legitimately be absent |
| NFR-RE-05 | No crash-recovery window where a failure persists without its Mode-1 recovery | Same-tick snapshot atomicity (SRS FR-016) — verified against `scheduler_loop.rs:338,547-551` |

---

## Durability / Crash-Resume

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-DU-01 | New `TaskNode` fields round-trip through the existing SQLite/journal persistence path | `#[serde(default, skip_serializing_if = "Option::is_none")]` on both `timeout` and `recovery` — a graph persisted before this feature existed deserializes with both fields `None`, no data loss, no migration required for the graph-data fields (SRS FR-019) |
| NFR-DU-02 | No new resume-time logic | Explicitly not added — the same-tick snapshot atomicity guarantee (NFR-RE-05) makes it unnecessary; resume rebuilds `running` from persisted `TaskStatus::Running` entries exactly as it does today (`crates/zeph-orchestration/src/scheduler/mod.rs:389-430`), unmodified by this feature |
| NFR-DU-03 | New config field forward/backward compatible | `default_idle_timeout_secs: Option<u64>` with `#[serde(default)]` and a dedicated `MIGRATIONS` step deserializes to `None` for configs written before this feature existed (SRS FR-018) |

---

## Async Supervision (spec-039 Compliance)

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-AS-01 | Zero new `tokio::spawn()` call sites | Recovery is a synchronous data mutation inside the already-synchronous `propagate_failure()`/`scheduler.tick()` call chain; timeout enforcement reuses the existing `tokio::time::timeout` pattern already present on the `RunInline` `select!` (adding a branch, not a new spawn) and the existing `check_timeouts()`/`wait_event()` polling loop. Per `[[039-background-task-supervisor/spec]]`'s binding NEVER section, no new detached task is created |
| NFR-AS-02 | No lock held across `.await` | Neither the timeout-override lookup nor the recovery mutation introduces any lock (`parking_lot` or otherwise) — both operate on data already owned by `&mut self`/`&mut TaskGraph` inside a synchronous call |
| NFR-AS-03 | No `*_provider` field required | Recovery performs no LLM call — `state_injection` is a planner-authored literal string, not a generated value. This project's multi-model design principle (every subsystem that calls an LLM must expose a `*_provider` field) does not apply because no LLM call exists on this path |

---

## Observability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-OB-01 | Timeout-cause disambiguation | When a task times out, the log/trace record names which mechanism fired (per-task `run_timeout_secs` override vs. graph-global `task_timeout` fallback vs., in a future Alt A build, `idle_timeout_secs`) — never an aggregate/ambiguous flag |
| NFR-OB-02 | Recovery invocation is traced | Every Mode-1 recovery application (`propagate_failure()`'s new branch) is wrapped in or logged via `tracing::info_span!`/`tracing::warn!` following the `<crate_short>.<subsystem>.<operation>` naming convention (e.g. `orchestration.dag.recover_task`), consistent with this project's instrumentation requirement |
| NFR-OB-03 | `validate()` guard failures name the exact defect | The FR-011 reject error names the offending task index/id and both conflicting fields; the FR-012 warn names the task and its effective failure strategy — never a generic validation failure |
| NFR-OB-04 | Idle-timeout no-op is loudly surfaced, not silent | `--init` wizard help text and `config.toml` comment both state "reserved — not yet enforced (see follow-up)" for `idle_timeout_secs` (per-task) and `default_idle_timeout_secs` (global) — critic finding M-b (SRS FR-005, FR-018) |

---

## Maintainability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-MA-01 | `TimeoutPolicy`/`RecoveryAction` follow the existing per-task override pattern | Structurally consistent with `failure_strategy: Option<FailureStrategy>` and `max_retries: Option<u32>` already on `TaskNode` — no new override idiom introduced |
| NFR-MA-02 | `RecoveryAction` is additively extensible | `route_to` (Mode 2, deferred) can be added later as an additional `#[serde(default)]` field on the same struct without a breaking schema change or a new type |
| NFR-MA-03 | All new `pub` items carry doc comments | Per CLAUDE.md's rustdoc requirements; `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-orchestration -p zeph-config` passes clean |
| NFR-MA-04 | Load-bearing bypass is documented in code, not only in this spec | The `ready_tasks()` `Ready`-arm dependency-completion bypass (`dag.rs:185-191`) gains a doc comment explaining its role in the recovery unblock path (SRS FR-020), so a future refactor does not silently break recovery semantics |

---

## Compatibility / Scope Boundary

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-CO-01 | Default (all-`None`) behavior is byte-for-byte unchanged | Verified by NFR-RE-01's regression test (BRD SC-01) |
| NFR-CO-02 | `FailureStrategy` enum and its `Abort`/`Skip`/`Ask` arms are unchanged | Only the `Abort` arm and the retry-exhausted branch of the `Retry` arm gain a conditional recovery check before falling through to their existing behavior; `Skip` and `Ask` arms are not touched |
| NFR-CO-03 | No scheduler/subagent-spawn architecture change | This feature is scoped entirely to `TaskNode` data, `dag.rs` failure-propagation logic, `tick/mod.rs` timeout evaluation, one `RunInline` `select!` branch, and `validate()` — `DagScheduler`'s dispatch/spawn machinery, `zeph-subagent` grants, and transcripts are untouched |
| NFR-CO-04 | Mode 2 and Alt A remain schema-compatible follow-ups | Deferring them does not require a breaking change to ship later — `RecoveryAction.route_to` and `TimeoutPolicy`/`OrchestrationConfig`'s idle-timeout enforcement are additive when implemented |

---

## Usability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-US-01 | `validate()` errors and warnings are actionable | Name the specific task, the specific conflicting/inert configuration, and (for the reject case) which fields must not co-occur |
| NFR-US-02 | `--init` wizard framing for `default_idle_timeout_secs` | Prompt text states the field is not yet enforced before accepting a value, preventing an operator from assuming idle-based kills are active (NFR-OB-04) |
