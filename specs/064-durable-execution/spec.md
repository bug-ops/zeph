---
aliases:
  - Durable Execution Spec
  - spec-064
tags:
  - sdd
  - spec
  - durable-execution
  - journal
  - reliability
created: 2026-05-30
status: approved
related:
  - "[[specs/001-system-invariants/spec]]"
  - "[[specs/009-orchestration/spec]]"
  - "[[specs/018-scheduler/spec]]"
  - "[[specs/031-database-abstraction/spec]]"
  - "[[specs/044-subagent-lifecycle/spec]]"
  - "[[specs/038-vault/spec]]"
  - "[[specs/039-background-task-supervisor/spec]]"
  - "[[specs/029-feature-flags/spec]]"
  - "[[specs/078-agent-persistence/spec]]"
  - "[[specs/063-worktree-subsystem/spec]]"
  - "[[constitution]]"
---

# Spec-064: Native Durable Execution Layer (`zeph-durable`)

**GitHub:** #4707 (epic) — child issues #4944–#4954 (milestone M28)
**Branch:** `feat/m28/{issue}-durable-*` (one branch per child issue)
**Crate:** `zeph-durable` (new Layer 0), plus integration adapters in `zeph-core`,
`zeph-orchestration`, `zeph-scheduler`, `zeph-subagent`

---

## Summary

This spec defines a lightweight, in-process durable execution layer for Zeph. The layer journals
the *control flow* of an execution (individual steps, their inputs and outputs, promises, timers)
so that a crashed or interrupted execution can be resumed at the point of failure rather than
restarted from scratch.

The design provides:

- A journal abstraction backed by a dedicated SQLite (or Postgres) database, with an optional
  feature-gated Restate backend for cloud/multi-process deployments.
- A `DurableContext` facade with `step()`, `parallel()`, `promise()`, and `sleep_until()` — the
  entire durable API is `&self`-based, concurrent-safe by construction.
- An explicit `EffectClass` contract per step (`Idempotent` / `AtLeastOnce` /
  `ExactlyOnceGuarded`), with per-intent `OnAmbiguous` policy that is a **construction-time error**
  for destructive/unspecified steps.
- A background `JournalWriter` actor that decouples all DB writes from calling-task await chains,
  with group-commit for buffered entries and ACK-await for exactly-once intents.
- AEAD payload encryption (XChaCha20-Poly1305) default-on, keyed from the vault, with AAD bound
  to step identity.
- Integration adapters at four priority levels: P1 agent tool-loop, P2 orchestration `/plan
  resume`, P3 scheduler exactly-once, P4 subagent durable promise.

**Non-goals (v1):** saga/`CompensatingStep`, automatic crash-recovery (no auto-reload on startup),
online journal migration between backends, multi-tenant isolation, distributed cross-process
coordination beyond what Restate provides. See NEVER section.

---

## Key Invariants

**INV-1 — Layer 0 infrastructure: no business-logic dependencies.**
`zeph-durable` MUST NOT depend on `zeph-llm`, `zeph-memory`, `zeph-core`, `zeph-sanitizer`, or
any business-layer crate. It sees opaque serialized payloads, not domain types. Domain meaning
lives in thin adapter modules inside each consuming crate.

**INV-2 — Deterministic StepId assignment is structural, not temporal.**
`StepId` is assigned at the moment `step()` is *called* (the Nth call in program order is StepId
N), never at completion. The `next_step: AtomicU32` counter increments via `fetch_add` in
argument order. For `parallel()`, a contiguous `[base..base+n)` block is reserved upfront so each
child future gets a stable id before any of them is polled. This makes StepId independent of
concurrent completion order.

**INV-3 — Replay MUST verify descriptor-fingerprint equality per StepId.**
Before returning a journaled result for StepId N, the `ReplayCursor` MUST assert the replayed
step's `StepDescriptor` fingerprint matches the fingerprint stored in the journal at StepId N
(one BLAKE3 compare). On mismatch, emit `DurableError::ReplayDivergence` and fall back to
discard-and-restart-fresh (today's pre-durable behavior). Never return a journaled result for
a structurally different step — that is silent corruption. A non-determinism lint/`#[durable]`
macro is post-v1 defense-in-depth; this fingerprint check is the v1 safeguard.

**INV-4 — EffectIntent flush-before-commit ordering.**
Before the `JournalWriter` commits any `ExactlyOnceGuarded` `EffectIntent`, it MUST flush all
causally-preceding `AppendBuffered` entries. This ensures the `ReplayCursor` always rebuilds a
contiguous committed prefix. A replayed `EffectIntent` can never appear ahead of the buffered
steps that logically precede it.

**INV-5 — No payload bytes in spans, logs, or CLI output without --reveal.**
Spans MUST NOT include payload bytes or resolver tokens. `zeph durable show`/`inspect` output is
redacted (metadata only) by default. Raw `PromiseId` is treated as semi-sensitive (log only its
hash). Decrypted payload content requires an explicit `--reveal` flag with a printed warning.

**INV-6 — Secrets referenced by vault key, never by value.**
A consumer MUST NOT pass vault-resolved secret material into `ctx.step()` payload or
`op_fingerprint`. Secrets are referenced by vault key name, never by resolved value — mirrors the
`ZEPH_DATABASE_URL`-as-key pattern and spec-050 `BoundSecret<Op>` direction. The `op_fingerprint`
MUST be derived from non-secret descriptors only (tool name and non-secret args).

**INV-7 — Each payload seal uses a fresh CSPRNG nonce.**
Every call to `PayloadCipher::seal` MUST generate a fresh random 24-byte `XNonce`; the stored
blob layout is `nonce || ciphertext || tag`. Nonce reuse under a fixed key is forbidden. A
deterministic nonce derived from the AAD (even when the AAD is unique per entry) is NOT acceptable
because a re-sealed entry (retry, migration) would reuse the nonce.

**INV-8 — AEAD authentication is required for shared-DB and Restate deployments.**
For single-user SQLite, AEAD is default-on and defense-in-depth; it may be disabled by explicit
`durable.encrypt_payload = false` (dev override, generates a startup WARN). For Postgres/shared
DB/Restate, disabling AEAD is FORBIDDEN: the DB-file trust boundary does not hold in multi-client
environments.

**INV-9 — `PromiseId` is NOT a bearer capability.**
Promise resolution requires a separate 32-byte high-entropy resolver token (zeroized, stored as
BLAKE3 hash in `durable_promises.resolver_token_hash`, constant-time compared on `resolve`).
Resolver tokens are bound to `(promise_id, execution_id)`. A2A-path resolution inherits
`AuthConfig` bearer auth. HITL resolution is operator-channel-only; the LLM MUST NOT be able to
resolve its own pending promises.

**INV-10 — No double-persist: replayed `Idempotent` steps skip side effects.**
A replayed `DurableStep` with `EffectClass::Idempotent` returns the journaled result; the `op`
closure is never invoked. This reconciles with spec-057 invariant "NEVER double-persist": a
replayed `persist_message` step returns the journaled `MessageId` instead of re-inserting.

**INV-11 — `max_payload` enforced on read (not only append).**
A payload exceeding `durable.max_payload_bytes` (default 1 MB) on read fails closed with
`DurableError::PayloadTooLarge`; it never panics. Recursion and allocation for serde
deserialization are bounded. Wire format is versioned via a `payload_version` discriminator
field. Corrupt or tampered entries → `DurableError::Decode`, fail closed.

**INV-12 — JournalWriter is supervised; ACK awaits time out.**
The `JournalWriter` tokio task MUST be tracked via `zeph-common::TaskSupervisor` (the unified `JoinSet` wrapper, spec-039) supervised by the daemon supervisor
(consistent with spec-039 background-task rules). On panic/restart, the writer re-reads the last
committed `JournalSeq` and resumes. `AppendAcked` MUST time out after `journal_ack_timeout_ms`
(default 5000 ms) with `DurableError::JournalUnavailable`; on timeout the calling path degrades
to non-durable mode and emits `WARN`. The agent loop MUST NOT hang indefinitely awaiting a
stalled writer.

**INV-13 — `ReplayDivergence` + already-committed `ExactlyOnceGuarded` interaction.**
When `DurableError::ReplayDivergence` is raised and the execution is discarded for a fresh run,
guarded effects from the aborted execution that already have a committed `StepResult` are
recognized via their `IdempotencyKey` on the fresh run. The pre-existing `StepResult` is returned
(the effect is NOT re-fired). This prevents re-applying a destructive effect that succeeded before
the divergence was detected.

**INV-14 — `durable.db` is a dedicated SQLite file (SQLite mode only); migrations live in
`zeph-db`.**
`zeph-durable` owns its own `DbPool` instance on a separate database file, namespaced by the main
DB's full file name so two distinct memory databases sharing a directory never collide on one
journal file (default: `{data_dir}/{main_db_file_name}.durable.db`, e.g. `zeph.db.durable.db` for
`memory.sqlite_path = zeph.db`; a pre-existing bare `{data_dir}/durable.db` from before this
namespacing was introduced is preferred when present, so upgrades do not orphan it — see #5553).
This eliminates cross-writer `BEGIN IMMEDIATE` contention with the main
DB's five hot writers (messages, graph blob, jobs, memory, audit). However, `zeph-durable` NEVER
owns migrations: all durable schema (the four `durable_*` tables) is added as numbered `.sql`
files in `zeph-db/migrations/sqlite/` and `zeph-db/migrations/postgres/` (matching file counts,
schema-equivalent — invariant §13). They are applied via `zeph_db::run_migrations(&durable_pool)`
against the dedicated `durable.db` pool — one call to the single, centralized migration runner
(031 §12 single source of truth). Under Postgres, the journal tables share the server but use the
`durable_` prefix; MVCC removes the contention concern.

**Migration-ownership decision (031-compliant):** `zeph_db::run_migrations` applies the whole
`zeph-db/migrations/` directory, so `durable.db` will also materialize other main-DB table DDL
(they become empty tables). This is the accepted MVP-simplest approach — harmless, as those tables
are unused in that file. Future option (b): add a scoped `zeph_db::run_durable_migrations(pool)`
helper; deferred post-v1. Both options comply with 031.

**INV-15 — An `ExecutionId` is process-exclusive while held (#6122).**
Two processes can legitimately derive the same `ExecutionId` for a fresh execution — the P1
adapter keys it on `(ConversationId, sqlite_path)` (INV-14's #5553 fold), so two CLI instances
pointed at the same `memory.sqlite_path` and the same (first-ever) `ConversationId` always agree
on the id. `LocalBackend::open_execution` alone (a plain SELECT-then-INSERT, no transaction) does
not stop two such processes from both observing "no existing row", both inserting, and both then
driving `next_step` from `0` against the same journal — corrupting it and surfacing as
`ReplayDivergence`/`ReplayIntegrity` on whichever process loses the race. `open_execution_exclusive`
closes this by taking a non-blocking, `flock(2)`-backed advisory lock on `id` (`ExecutionLock`, a
lock file named after the execution's UUID) before the row check; the loser gets
`DurableError::ExecutionLocked` instead of racing. The lock is held for the caller's
`DurableContext` lifetime and released on drop — `flock(2)` locks are released by the kernel when
the holding process's file descriptors close (including on `SIGKILL`), so a hard-killed process
never leaves a stale lock. Unlike `zeph_common::pidfile::PidLockGuard` (the primitive backing
`PidGuard`/`PidFile`), `ExecutionLock` never unlinks its lock file on drop — it is a permanent
sentinel, matching `zeph-session::log::SessionEventLog`'s own `AdvisoryLock`. `PidLockGuard`
unlinks-then-closes, which is safe for a pid file acquired once at daemon startup but is a real
`flock`+`unlink` TOCTOU hazard under the much higher per-conversation-turn contention this lock
sees. SQLite only: a `:memory:` database, a backend built via `LocalBackend::new` from a
caller-supplied pool, or a Postgres deployment has no derivable lock directory (a Postgres
connection URL may embed credentials and is never used to mint a directory name) and
`open_execution_exclusive` degrades to unenforced exclusivity there (mirrors
`SessionEventLog::open_exclusive`'s non-Unix degrade). Callers that need genuine crash-resume
(re-deriving the *same* `ExecutionId` after their own process restarts) are unaffected: the lock
only rejects a *second, concurrent* holder — a sequential resume by the same logical session
re-acquires the (now-released) lock normally. Track A alone is sufficient: `PayloadAad` already
binds `execution_id` (cipher.rs) and `IdempotencyKey::derive` already folds `execution_id` +
`step_id` (ids.rs), so an `op_fingerprint`/content salt on top would add no defense — the AAD/
`IdempotencyKey` collision this invariant's introduction considered was only ever possible when
`ExecutionId` itself collided, which this lock now prevents structurally.

**INV-16 — Reopening an execution un-finalizes ALL terminal statuses identically (#6254).**
`open_execution`/`open_execution_exclusive`'s reopen path MUST reset `status='running'` and clear
`finalized_at` for a row found in ANY terminal status — `completed`, `failed`, OR `aborted` — not
just `completed`/`failed` as in the pre-#6254 behavior. The deciding fact for prunability is
"is this execution being actively reopened right now", never "which terminal status produced the
row". Leaving `aborted` untouched on reopen (the pre-#6254 behavior, justified at the time because
the only `aborted` producers were rare, immediately-redriven divergence-recovery/`StepCapExceeded`
aborts) becomes unsafe once the crash-orphan sweep (INV-17) makes `aborted` the common outcome of a
resumable crash: a resumed execution whose row keeps `finalized_at` set is prunable out from under
the active resume — the exact hazard the completed/failed un-finalize was built to prevent. Un-
finalizing `aborted` on reopen is strictly safer for the pre-existing divergence-recovery case too
(it now also protects that fresh re-drive from prune) and does not change replay-cursor
fresh-vs-resume selection, which is independent of the `status` column.

**INV-17 — A crash-orphaned `running` execution is reclaimed only after its liveness is verified
via the INV-15 advisory lock, never by staleness alone (#6254).**
A background sweep (`Journal::sweep_orphans`, folded into the existing retention tick, running
before `prune`) MAY hard-abort a row matching `status='running' AND finalized_at IS NULL AND
updated_at <= now - stale_running_after_secs` (config: `RetentionPolicy::stale_running_after_secs`,
default 3600s; `0` disables the sweep) **only** after a non-blocking try-acquire of that
execution's INV-15 `ExecutionLock` succeeds. `ExecutionLocked` (lock held by a live owner) MUST
short-circuit to skip — staleness of `updated_at` alone is never sufficient grounds to abort,
because `updated_at` is not bumped on every journal append, and a genuinely active-but-idle
execution (a long single step, a parked HITL promise, a multi-hour scheduled-job body) would
otherwise be false-aborted. The abort `UPDATE` runs while the sweep still holds the acquired lock
(guard: `WHERE execution_id=? AND status='running' AND finalized_at IS NULL`), which makes it
race-free against a concurrent `open_execution_exclusive` reopen for the same id — the two can
never both proceed, because both require the same non-reentrant flock. The sweep never retries or
resumes an orphan; it only marks it `aborted` so the pre-existing crash-resume path
(`ensure_session_durable_ctx` → `open_execution` replay) reclaims it on next legitimate open.
Sweep and prune are ordered on the same tick — the sweep MUST run before `prune()` so a
just-aborted orphan is visible to that same tick's TTL check (informational: with
`ttl_failed_secs=0` this makes a freshly-swept orphan prune-eligible on the same tick — intended,
not a bug, since `0` already means "prune failed/aborted immediately"). Correctness rests on every
production execution kind holding its flock while live: `AgentTurn` already opens via
`open_execution_exclusive`; `ScheduledJob` and `DagRun` are converted to it by this change (see
P2/P3 adapter notes) specifically to make flock-liveness universal. On backends where
`lock_dir=None` (`:memory:`, Postgres, non-Unix — the same class INV-15 already degrades), the
sweep is a documented no-op, never a staleness-only abort — see NEVER and Non-Goals.

---

## NEVER

- **NEVER** reuse a nonce for `PayloadCipher::seal` under the same key.
- **NEVER** run `zeph-durable` without payload AEAD under Postgres, shared DB, or Restate.
- **NEVER** allow the LLM to call a promise-resolution API; HITL resolution is operator-only.
- **NEVER** return a journaled result without first verifying the descriptor fingerprint matches
  (INV-3). Silent result substitution on StepId mismatch is never acceptable.
- **NEVER** add a `sqlx::migrate!` or own `.sql` migration files inside `zeph-durable`. All durable
  schema files go in `zeph-db/migrations/{sqlite,postgres}/`; they are applied by calling
  `zeph_db::run_migrations` against the dedicated `durable.db` pool — the single centralized
  migration runner (031 §12). Owning a separate migrator would create a divergent source of truth.
- **NEVER** journal a domain type or resolved secret in the step payload; journal opaque
  pre-serialized bytes passed by the consumer adapter. Consumers sanitize before calling `step()`.
- **NEVER** call `journal.prune()`, `journal.sweep_orphans()`, or any other bulk-write on the step
  dispatch hot path. Pruning, orphan reclamation, and compaction run exclusively in a background
  task on a timed interval.
- **NEVER** consume `Box<dyn ExecutionBackend>` on the hot path. Use `DurableBackendEnum` with
  enum dispatch — consistent with `AnyProvider`/`AnyChannel` precedent.
- **NEVER** add a `restate` feature to the `full` bundle. Restate requires an external server;
  it is environment-specific (analogous to `postgres`, `metal`). It lives in `server` only.
- **NEVER** accept a `StepDescriptor` for `ExactlyOnceGuarded` without an explicit `on_ambiguous`
  field when the effect is classified as destructive or security-relevant. The absence of an
  `on_ambiguous` for such steps is `DurableError::AmbiguityPolicyRequired` at construction time.
- **NEVER** double-persist: a replayed step MUST return the journaled result, not invoke `op`.
- **NEVER** include raw payload bytes, resolver tokens, or execution keypaths in tracing spans,
  log lines, or CLI output without `--reveal` (INV-5).
- **NEVER** claim P2 fixes automatic crash-recovery. The durable journal fixes the replan-budget
  reset on the existing `/plan resume` user command path only. Auto crash-recovery is not wired.
- **NEVER** drive an `ExecutionId` an adapter did not open through `open_execution_exclusive`
  (INV-15) when two processes could plausibly derive the same id (e.g. any id keyed on
  externally-observable state like `ConversationId` rather than a runtime-minted `UUIDv7`). Calling
  the unsynchronized `open_execution` directly from such an adapter reopens the #6122 race.
- **NEVER** convert `zeph-durable/src/writer.rs` or `handle.rs`'s `open_execution` call sites to
  `open_execution_exclusive`. These run in the same process that already holds the driving
  adapter's `ExecutionLock`; `flock(2)` is associated with the open file description, so a second
  `open()`+`flock` on the same lock file from the same process self-errors (`WOULDBLOCK` /
  `ExecutionLocked`) against the live execution it is trying to serve, rather than protecting it
  (#6254). Only the two named adapter entry points (`zeph-scheduler`'s `fire_with_durable`,
  `zeph-orchestration`'s `journal_budget`) convert — see INV-17 and the P2/P3 adapter notes. Every
  other `open_execution` call stays plain: it is either covered by the adapter's own lock for
  cross-process liveness, or would deadlock if re-locked in-process.
- **NEVER** ship a staleness-only crash-orphan abort (no INV-15 flock liveness check) for
  `lock_dir=None` backends (Postgres, `:memory:`, non-Unix). Without a liveness signal, staleness
  of `updated_at` alone cannot distinguish a dead execution from a live one — see INV-17. This is
  an explicit Non-Goal of #6254 (documented no-op + warn-once); backend-agnostic reclamation for
  these backends requires a different mechanism (heartbeat-timeout based) and is tracked as a
  separate follow-up, not shipped in this PR.

---

## Scope & Non-Goals

### In Scope (v1)

- New crate `zeph-durable` at Layer 0 (analogous to `zeph-db`, `zeph-common`).
- Journal abstraction: append-only, ordered, idempotent on replay; dedicated SQLite `durable.db`
  (LocalBackend) plus optional feature-gated Restate backend.
- `DurableStep` primitive: record (operation, result); on replay skip execution and return the
  journaled result.
- `EffectClass` contract per step with explicit `OnAmbiguous` policy (per-intent sub-class).
- Idempotency keys (BLAKE3, domain-separated) for deduplicating non-idempotent effects.
- Durable promises (`DurablePromise<T>`) for external completion: HITL, A2A async, subagent
  result. Resolver-token authentication.
- Durable timers (`DurableTimer`): wake-at-time persisted across restart.
- AEAD payload encryption (XChaCha20-Poly1305) with vault-keyed cipher injected at the binary.
- `JournalWriter` background actor: mpsc channel, group-commit, ACK for exactly-once, supervised.
- `ReplayDivergence` fingerprint guard.
- `ReplayCursor` with range-read segmentation (O(segment) memory at resume).
- Integration adapters:
  - P1: agent tool-loop in `zeph-core` (`src/agent/tool_execution/tier_loop.rs`) (explicit loop rewrite, LLM gate).
  - P2: orchestration `/plan resume` path, restore replan/lineage/predicate counters.
  - P3: scheduler exactly-once job fire via `JobStore.record_run()` seam.
  - P4: subagent durable spawn/await via `DurablePromise<SubagentResult>`.
- Journal retention and compaction (background sweep, per-execution step cap).
- Crash-orphan reclamation (#6254): a flock-verified staleness sweep (`Journal::sweep_orphans`)
  that hard-aborts `running` executions whose owner process died without finalizing, folded into
  the existing retention tick. See INV-16, INV-17, and the dedicated Retention & Compaction
  subsection.
- Mandatory integration points: `[durable]` config, `zeph durable` CLI, TUI `DurableView`,
  `--init` wizard, `--migrate-config`, testing playbook, coverage-status rows.
- 10 criterion benchmarks + `bench_step_run_exactly_once_n ≤ 5 ms @ N=5` CI regression gate.

### Non-Goals (v1)

- Saga / `CompensatingStep` (multi-effect rollback). Multi-effect executions are NOT atomic in v1;
  partial application is possible on failure. No automatic compensation.
  `// TODO(post-v1): CompensatingStep for transactional multi-effect rollback`
- Automatic crash-recovery on process start (no auto-reload and resume). P2 fixes only the
  explicit `/plan resume` user command.
- Online journal migration between backends (LocalBackend ↔ Restate).
- Distributed cross-process coordination beyond what Restate provides as an opt-in backend.
- Multi-tenant isolation.
- Aggressive journal compaction beyond checkpoint-fold and the background TTL sweep.
  `// TODO(post-v1): aggressive mid-execution compaction for very long agent sessions`
- Replacing existing persistence (`zeph-agent-persistence` messages, orchestration
  `GraphPersistence`, scheduler `JobStore`, subagent transcripts). The durable layer
  *complements* them with an execution-flow journal; it does not subsume them.
- **Backend-agnostic crash-orphan reclamation for `lock_dir=None` backends** (Postgres, `:memory:`,
  non-Unix) (#6254). The crash-orphan sweep requires the INV-15 advisory flock as its liveness
  signal and is a documented no-op there (warn-once at startup: `"durable: crash-orphan sweep
  requires an on-disk advisory-lock dir; orphan reclamation disabled for this backend
  (Postgres/:memory:/non-Unix)"`). A backend-agnostic mechanism (heartbeat-timeout based, "option
  c" in the design debate) is a distinct future mechanism — it is NOT shipped in this PR and is
  tracked as a separate follow-up issue filed after this PR merges. Shipping a staleness-only abort
  for these backends in the meantime is explicitly forbidden (see NEVER) — it would reintroduce a
  false-abort-of-a-live-execution defect on the very backends where liveness cannot be verified.
  `// TODO(post-v1): backend-agnostic (Postgres) crash-orphan reclamation via heartbeat timeout`

---

## Crate & Module Layout

```
zeph-durable  (Layer 0 — infrastructure, analogous to zeph-db)
Cargo.toml    # dep: zeph-config, zeph-db, serde, thiserror, tokio, tracing, blake3 (chacha20poly1305 lives in zeph-core)
src/
  lib.rs              # pub re-exports; crate-level //! docs; sealed module
  sealed.rs           # Sealed marker (private supertrait for ExecutionBackend)
  ids.rs              # newtypes: ExecutionId, StepId, JournalSeq, IdempotencyKey, PromiseId, TimerId
  journal.rs          # Journal trait, JournalEntry, EntryKind, ExecutionStatus
  step.rs             # DurableStep<T>, StepOutcome<T> typestate, StepDescriptor
  effect.rs           # EffectClass, OnAmbiguous, EffectIntentSubClass
  promise.rs          # DurablePromise<T>, DurableHandle (resolver entry point)
  timer.rs            # DurableTimer, DurableTimerService (background poll)
  handle.rs           # DurableContext (&self API, AtomicU32, ReplayCursor, JournalWriterHandle)
  backend.rs          # sealed ExecutionBackend trait, DurableBackendEnum, BackendCapabilities
  backend/
    local.rs          # LocalBackend (durable.db; always compiled)
    restate.rs        # RestateBackend (feature = "restate"; maps to Restate SDK)
  replay.rs           # ReplayCursor, ReplayDivergence check, range-read cursor
  writer.rs           # JournalWriter actor, JournalMsg enum, group-commit, ACK protocol
  cipher.rs           # PayloadCipher trait, PayloadAad, CipherError
  retention.rs        # compaction/prune, crash-orphan sweep (#6254), in-execution step cap
  config.rs           # re-exports DurableConfig/RetentionPolicy/DurableBackend from zeph-config;
                      #   owns the EncryptionGate + encryption_gate AEAD policy (free fn)
  error.rs            # DurableError (thiserror)
# NO migrations/ directory — durable schema files live in zeph-db/migrations/{sqlite,postgres}/
# and are applied via zeph_db::run_migrations against the dedicated durable.db pool.
```

**Layer placement rationale.** Consumers span L0c (`zeph-scheduler`), L2 (`zeph-subagent`), and
L3 (`zeph-orchestration`, `zeph-core`). A crate used by all four must sit at Layer 0.
`zeph-durable` is a pure infrastructure primitive — no business logic — exactly analogous to
`zeph-db` and `zeph-common` in the existing layered DAG.

**Migration ownership (031-compliant).** `zeph-durable` opens its own `DbPool` on a dedicated
`durable.db` file (the pool is opened using `zeph_db::DbConfig`/`DatabaseDriver` — the
`zeph-scheduler` pool-creation precedent is valid). Schema files for the four `durable_*` tables
are added as numbered migration files in `zeph-db/migrations/sqlite/` and
`zeph-db/migrations/postgres/` (matching counts, schema-equivalent — invariant §13). They are
applied by calling `zeph_db::run_migrations(&durable_pool)` on the dedicated pool — the single,
workspace-wide migration runner (031 §12 "single source of truth"; the only `sqlx::migrate!` in
the workspace lives in `zeph-db/src/migrate.rs`). `zeph-durable` itself contains NO `.sql` files
and NO `sqlx::migrate!`. The `JobStore` precedent cited in rev-C.3 covers the *pool* only:
`JobStore::init` calls `zeph_db::run_migrations` — it does not own schema files
(`zeph-scheduler/src/store.rs:137`; its schema is `051_scheduler_jobs.sql` inside `zeph-db`).

**Config placement (C6 #4949 reconciliation).** The pure-data `DurableConfig`, `RetentionPolicy`,
and `DurableBackend` live in `zeph-config` — the single source of truth for every subsystem's
config, exactly like `OrchestrationConfig`. This keeps the aggregate `Config` free of the
`zeph-db`/`sqlx` dependency tree (12 leaf crates depend on `zeph-config` without `zeph-db`; pulling
it in would be a workspace-wide compile regression). `zeph-durable` depends on `zeph-config` and
re-exports those types, so `zeph_durable::DurableConfig` still resolves and the engine APIs
(`DurableContext`, `JournalWriter`) consume the same type the root `Config` holds — no duplication,
no conversion. The AEAD enforcement policy (`EncryptionGate` + `encryption_gate`, a free function
returning `DurableError`) stays in `zeph-durable` next to the cipher contract and error type
(data/policy separation). `zeph-config` is pure-data and below `zeph-durable` in the layer DAG, so
this introduces no cycle and does not violate INV-1 (`zeph-config` is not a business-layer crate).

---

## Core Types

### Newtypes (ids.rs)

All newtypes: `#[derive(Debug, Clone, ...)]`, serde-serializable, private fields, smart
constructors. No raw `String`/`i64` crosses the journal boundary.

| Newtype | Wraps | Construction | Purpose |
|---------|-------|-------------|---------|
| `ExecutionId` | `Uuid` (v7) | `ExecutionId::new()` | One durable execution. Runtime-minted; NEVER consumer-supplied for a fresh execution. |
| `StepId` | `u32` | via `DurableContext` atomic counter | Position of a step in an execution; deterministic across replays. |
| `JournalSeq` | `i64` (DB autoincrement) | DB-assigned | Global append order — the durability anchor. |
| `IdempotencyKey` | `[u8; 32]` (BLAKE3) | `IdempotencyKey::derive(execution_id, step_id, op_fingerprint)` | Domain-separated dedup key (see §Idempotency Key Derivation). |
| `PromiseId` | `Uuid` v7 | `PromiseId::new()` | External-completion handle reference. Not a bearer capability. |
| `TimerId` | `Uuid` v7 | `TimerId::new()` | Durable wake handle. |
| `ExecutionKind` | closed enum | — | `AgentTurn`, `DagRun`, `ScheduledJob`, `SubagentSession`, `Custom(&'static str)`. Closed enum prevents typos; used by retention policy. |

### Journal trait (journal.rs)

```rust
trait Journal: Send + Sync {
    async fn append(&self, entry: JournalEntry) -> Result<JournalSeq, DurableError>;
    async fn read_execution(&self, id: ExecutionId) -> Result<Vec<JournalEntry>, DurableError>;
    async fn read_execution_range(
        &self, id: ExecutionId, from_step_id: u32, limit: usize,
    ) -> Result<Vec<JournalEntry>, DurableError>;
    async fn finalize(&self, id: ExecutionId, status: ExecutionStatus) -> Result<(), DurableError>;
    async fn prune(&self, policy: &RetentionPolicy) -> Result<u64, DurableError>;
    /// Crash-orphan reclamation (#6254): flock-verify and hard-abort stale `running` rows.
    /// Returns the count aborted. No-op (`Ok(0)`) on backends without a `lock_dir` (INV-17).
    async fn sweep_orphans(&self, policy: &RetentionPolicy) -> Result<u64, DurableError>;
}
```

`sweep_orphans` is dispatched through `DurableBackendEnum` exactly like `prune` — the retention
loop already holds `Arc<DurableBackendEnum>`. `LocalBackend` implements the real flock-probe logic
(INV-17); `RestateBackend` returns `Ok(0)` (Restate has its own crash-recovery semantics, out of
scope here).

`read_execution_range` is the path for long executions (DAG runs, agent sessions). The
`ReplayCursor` reads N steps ahead (default 100, configurable), re-queries as replay advances —
O(segment) memory. `read_execution` is retained for short executions.

`JournalEntry`:
```rust
struct JournalEntry {
    seq: Option<JournalSeq>,       // None before append; Some after DB assignment
    execution_id: ExecutionId,
    kind: ExecutionKind,
    step_id: StepId,
    entry: EntryKind,
    created_at_ms: i64,
}
```

`EntryKind` is a closed enum making illegal states unrepresentable. Exhaustive match on replay:

```rust
enum EntryKind {
    StepResult {
        idempotency_key: IdempotencyKey,
        payload: Bytes,            // AEAD-sealed; layout: nonce(24B) || ciphertext || tag
        effect: EffectClass,
        payload_version: u8,
    },
    EffectIntent {
        idempotency_key: IdempotencyKey,
        effect: EffectClass,
        hmac: Option<[u8; 32]>,    // HMAC of control entry for shared-DB / Restate (INV-8)
    },
    PromiseCreated {
        promise_id: PromiseId,
        resolver_token_hash: [u8; 32],   // BLAKE3 of the resolver token
        hmac: Option<[u8; 32]>,
    },
    PromiseResolved { promise_id: PromiseId, payload: Bytes },
    TimerArmed { timer_id: TimerId, due_at_ms: i64, hmac: Option<[u8; 32]> },
    TimerFired { timer_id: TimerId },
    Checkpoint { up_to_step: u32, snapshot: Bytes },
}
```

Control entries (`EffectIntent`, `PromiseCreated`, `TimerArmed`) have no ciphertext payload. For
shared-DB/Restate deployments, the `hmac` field is a row-level HMAC keyed from `ZEPH_DURABLE_KEY`
over `(execution_id, step_id, entry_kind, idem_key|promise_id|due_at)`. This closes the
`EffectIntent`-forgery attack vector (security HIGH-2b). For single-user SQLite the HMAC field is
`None` (DB-file trust boundary is the documented, accepted stance).

The HMAC key is not a second vault secret: it is a BLAKE3 `derive_key` subkey of the same
`ZEPH_DURABLE_KEY` used for the AEAD payload cipher, domain-separated by a fixed context string so
the two keys are cryptographically independent
(`zeph_core::durable::derive_control_hmac_key_b64`). It is resolved and attached
(`LocalBackend::with_hmac_key`) at the same production choke points and gated by the same
`shared_db`/`postgres://`-detection policy as the AEAD `encryption_gate` (INV-8): a single-user
local, non-shared database never resolves the key, and a declared/detected shared database fails
closed if `ZEPH_DURABLE_KEY` cannot be resolved from the vault. Every read of an `EffectIntent`
control entry recomputes the HMAC and compares it in constant time
(`blake3::Hash` equality, mirroring the promise resolver-token check); a mismatch — including a
missing HMAC on a keyed backend, or a *stamped* HMAC read by an unkeyed backend — fails closed with
`DurableError::ControlIntegrity`. `PromiseCreated` and `TimerArmed` carry the same `hmac` field on
the wire type but are not yet journaled by `LocalBackend` (promise/timer persistence lives in a
separate layer, see `promise.rs`), so HMAC computation and verification currently applies only to
`EffectIntent`.

This forgery guarantee depends on `shared_db`/`postgres://` being declared consistently across the
writer and every reader of a given journal file; a reader that disagrees (e.g. runs unkeyed against
a keyed writer's file) now fails closed as soon as it encounters any row carrying a stamped HMAC,
rather than silently trusting it as an ordinary unverified field.

### DurableContext (handle.rs) — `&self` API

```rust
struct DurableContext {
    execution_id: ExecutionId,
    kind: ExecutionKind,
    next_step: AtomicU32,                      // &self step() is sound: atomic fetch_add
    cursor: ReplayCursor,                      // built once at open(); read-only per lookup
    writer: JournalWriterHandle,               // mpsc::Sender<JournalMsg>
    cipher: Option<Arc<dyn PayloadCipher>>,    // injected at binary
}

impl DurableContext {
    async fn step<T, F, Fut>(
        &self,
        desc: StepDescriptor,
        op: F,
    ) -> Result<T, DurableError>
    where
        T: Serialize + DeserializeOwned + Send,
        F: FnOnce(StepHandle) -> Fut,
        Fut: Future<Output = Result<T, StepError>> + Send;

    fn parallel(&self) -> ParallelScope<'_>;
    async fn promise<T>(&self) -> DurablePromise<T>;
    async fn await_promise<T: DeserializeOwned>(&self, p: DurablePromise<T>)
        -> Result<T, DurableError>;
    async fn sleep_until(&self, due: SystemTime) -> Result<(), DurableError>;
}
```

`StepDescriptor`:
```rust
struct StepDescriptor {
    name: &'static str,
    effect: EffectClass,
    on_ambiguous: Option<OnAmbiguous>,   // REQUIRED for destructive ExactlyOnceGuarded steps
    op_fingerprint: Bytes,               // BLAKE3 of non-secret descriptors; combined with execution_id
}
```

`StepOutcome<T>` typestate (internal): `Live(T)` vs `Replayed(T)`. Consumers that MUST NOT
re-emit side effects (e.g. printing to channel) branch on it. `RuntimeLayer` uses the
`Replayed` discriminator to suppress double-printing already-emitted assistant output; replay
control does not flow through `RuntimeLayer`.

`parallel()` reserves a contiguous `[base..base+n)` StepId block upfront; each child `scope.step`
gets the next id from the reserved block. Ids are assigned eagerly at future-construction time,
before any future is polled — completion order is irrelevant. Children hold `&self` to
`DurableContext` and are independently pollable for `join_all`.

### EffectClass & OnAmbiguous (effect.rs)

```rust
enum EffectClass {
    Idempotent,
    AtLeastOnce,
    ExactlyOnceGuarded,
}

enum EffectIntentSubClass {
    CostBearingOrBoundaryIdempotent,  // paid LLM call with idempotency header → default Skip
    Destructive,                      // file delete, fund transfer, credential mutation → default Fail
    SecurityRelevant,                 // permission mutation, credential derivation → default Fail
    MoneyMoving,                      // financial transfer → default Fail
    Custom,                           // requires explicit on_ambiguous in StepDescriptor
}

enum OnAmbiguous {
    Skip,   // assume effect happened; safe for cost-bearing/boundary-idempotent
    Fail,   // surface to operator; required for destructive/security-relevant
    Rerun,  // assume effect did not happen; for misclassified idempotent-ish effects
}
```

**Construction-time policy rule:** a `StepDescriptor` for `ExactlyOnceGuarded` with a
`Destructive`, `SecurityRelevant`, or `MoneyMoving` sub-class that does not specify `on_ambiguous`
is rejected at construction with `DurableError::AmbiguityPolicyRequired`. The safety decision is
forced at the call site, not deferred to a runtime default.

Every ambiguous-window resolution (Skip, Fail, or Rerun) MUST emit a mandatory structured audit
record: `tracing::warn!` + audit sink with `execution_id`, `step_id`, `effect_class`, `idem_key`,
`on_ambiguous` value, and timestamp.

### IdempotencyKey Derivation (ids.rs)

```rust
impl IdempotencyKey {
    pub fn derive(
        execution_id: ExecutionId,
        step_id: StepId,
        op_fingerprint: &[u8],
    ) -> IdempotencyKey;
}
```

Uses BLAKE3 `derive_key` mode with a fixed domain-separation context string
(`"zeph-durable v1 idempotency-key 2026"`). The derivation input is
`len(execution_id) || execution_id || len(step_id_le4) || step_id_le4 || op_fingerprint` — length
delimited (injective). Naive concatenation is forbidden (attacker-controlled `op_fingerprint`
could collide). `execution_id` is runtime-minted (UUIDv7); `op_fingerprint` is a dedup
discriminator only, NEVER the sole trust basis for skipping a guarded effect.

### PayloadCipher (cipher.rs)

```rust
pub trait PayloadCipher: Send + Sync {
    fn seal(&self, plaintext: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError>;
    fn open(&self, ciphertext: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError>;
}

struct PayloadAad {
    execution_id: ExecutionId,
    step_id: StepId,
    entry_kind: EntryKindTag,
    idem_key: Option<IdempotencyKey>,
}
```

The concrete implementation lives in the binary (or `zeph-core`-side module), using
XChaCha20-Poly1305 from the `chacha20poly1305` crate (audited, `unsafe`-free, pure Rust, 192-bit
extended nonce). The key is vault-resolved from `ZEPH_DURABLE_KEY` (never inline TOML); the
binary constructs the concrete cipher and injects it as `Option<Arc<dyn PayloadCipher>>` at
`LocalBackend`/`RestateBackend` construction — exactly as `DbPool` is handed in.

**Stored blob layout:** `nonce(24 bytes) || ciphertext || Poly1305-tag(16 bytes)`.

**AAD binding:** `(execution_id, step_id, entry_kind, idem_key)` — a `StepResult` payload cannot
be moved to a different step or replayed under a different execution; a forged/moved entry fails
`open()` → `DurableError::ReplayIntegrity`, fail-closed.

**Key rotation:** prefix a 1-byte key-id/version field to the stored blob so `open` can select
among the current + one previous key during a rotation window. Alternatively, document that key
rotation requires draining in-flight executions first (all running executions must reach terminal
status before the old key is removed). The concrete rotation policy MUST be documented in
`docs/src/vault.md`.

### JournalWriter Actor (writer.rs)

```rust
enum JournalMsg {
    AppendBuffered(JournalEntry),
    AppendAcked(JournalEntry, oneshot::Sender<JournalSeq>),
    Flush(oneshot::Sender<()>),
}
```

The `JournalWriter` is a single background tokio task, tracked in a `JoinSet` supervised by the
daemon supervisor. It holds a dedicated write connection on `durable.db`. All DB appends route
through this actor.

**Message handling:**
- `AppendBuffered` — fire-and-forget for `Idempotent`/`AtLeastOnce`. Group-committed on a
  configurable flush interval (default `journal_flush_interval_ms = 10`). Acceptable for these
  classes: an un-flushed buffered entry lost on crash simply means the step re-runs on resume,
  which is safe by definition.
- `AppendAcked` — for `ExactlyOnceGuarded` intents/results. Writer commits, sends `JournalSeq`
  back on the oneshot. Calling task awaits ACK. Per INV-4, all causally-preceding buffered entries
  are flushed before this commit.
- `Flush` — turn-boundary drain; calling task awaits.

**Channel capacity:** `mpsc::channel` bounded at capacity = **1024**. On full channel:
`AppendBuffered` uses `try_send` and drops with `WARN` (acceptable for `Idempotent`/
`AtLeastOnce`); `AppendAcked` uses a separate priority path (Semaphore-gated send or separate
bounded channel) and MUST NOT drop — it waits up to `journal_ack_timeout_ms` before returning
`DurableError::JournalUnavailable`.

**ACK timeout:** `journal_ack_timeout_ms` default **5000 ms**. On timeout: calling path receives
`DurableError::JournalUnavailable`, degrades to non-durable mode (logs `WARN`, continues). The
agent loop MUST NOT hang indefinitely.

**Writer restart:** on panic/task-completion (caught by `JoinSet`), the supervisor restarts the
writer. On restart, the writer queries `MAX(seq)` from `durable_journal` to determine the last
committed `JournalSeq` and resumes.

**Observability:** the writer emits a `durable.journal.writer.queue_depth` gauge event at each
commit cycle for backpressure detection.

**Durability-on-return guarantee per EffectClass (C-N1):**
- `Idempotent` / `AtLeastOnce` (`AppendBuffered`): durable only after the next group-commit flush.
  In the ≤`journal_flush_interval_ms` window before flush, a crash loses the buffered entry. This
  is acceptable by class definition (re-run is safe). The spec makes this explicit; no caller
  should assume buffered appends are immediately durable.
- `ExactlyOnceGuarded` (`AppendAcked`): durable when the caller receives the oneshot ACK. The
  writer has committed the WAL before sending the ACK.

### ExecutionBackend (backend.rs)

```rust
pub trait ExecutionBackend: Journal + Send + Sync + crate::sealed::Sealed {
    async fn open(
        &self,
        id: ExecutionId,
        kind: ExecutionKind,
    ) -> Result<DurableContext, DurableError>;
    async fn resolve_promise(
        &self,
        id: PromiseId,
        resolver_token: &[u8; 32],
        payload: Bytes,
    ) -> Result<(), DurableError>;
    async fn due_timers(&self, now_ms: i64) -> Result<Vec<(ExecutionId, TimerId)>, DurableError>;
    fn capabilities(&self) -> BackendCapabilities;
}

struct BackendCapabilities {
    parallel_steps: bool,
    cross_process: bool,
    max_payload: usize,
}

enum DurableBackendEnum {
    Local(LocalBackend),
    #[cfg(feature = "restate")]
    Restate(RestateBackend),
}
```

`Sealed` (private supertrait) makes `ExecutionBackend` un-implementable outside the crate —
mirrors `zeph-agent-tools::Sealed` and the `AnyProvider`/`AnyChannel` closed-dispatch pattern.

**LocalBackend** (always compiled): journals to `durable.db`. `parallel_steps = true`. Zero new
mandatory deps beyond `zeph-db` + `chacha20poly1305`.

**RestateBackend** (`feature = "restate"`, `server` bundle only):
- Maps `step` → Restate journaled action, `promise` → awakeable, `timer` → durable sleep.
- `parallel_steps = false`. `parallel()` collapses to sequential durable wrapping, but the
  underlying native tool execution still runs concurrently (the durability wrapper serializes
  only the *recording*, not the I/O).
- Restate result ordering: `RestateBackend::parallel()` MUST commit results in reserved-StepId
  order (bounded `BTreeMap<StepId, Bytes>` buffer) to satisfy Restate's determinism requirement.
  `AtLeastOnce` parallel effects have a wider duplicate window under Restate than LocalBackend;
  `ExactlyOnceGuarded` is MANDATORY for any non-idempotent effect in a Restate parallel batch.
- **Mandatory TLS:** the Restate ingress URL MUST be `https://`. An `http://` URL is rejected
  at backend construction unless `restate.allow_insecure = true` (dev override, generates a
  startup WARN).
- **Vault-resolved ingress credentials:** `ZEPH_RESTATE_INGRESS_URL`, `ZEPH_RESTATE_API_KEY`
  (if required by the Restate server) are vault-resolved, never inline TOML — mirroring
  `ZEPH_DATABASE_URL`.
- **Awakeable callback → resolver-token:** the Restate → Zeph awakeable callback (promise
  resolution path) is governed by INV-9's resolver-token contract: the callback URL encodes the
  `resolver_token` as an opaque bearer; the server verifies the token via constant-time BLAKE3
  hash compare against `resolver_token_hash` in `durable_promises`.
- **AEAD required:** `RestateBackend` MUST NOT operate without a `PayloadCipher` (INV-8 for
  off-process).
- **cargo-deny gate:** the `restate` feature PR MUST pass `cargo deny check advisories` +
  license review on `restate-sdk` and its transitive dependency tree before merge.

### DurablePromise & DurableHandle (promise.rs)

```rust
struct DurablePromise<T> {
    id: PromiseId,
    resolver_token: Zeroizing<[u8; 32]>,   // high-entropy; zeroized on drop
    _t: PhantomData<T>,
}

// Cheaply cloneable, Send+Sync — the out-of-band resolver entry point
struct DurableHandle {
    backend: Arc<DurableBackendEnum>,
}

impl DurableHandle {
    async fn resolve<T: Serialize>(
        &self,
        id: PromiseId,
        resolver_token: &[u8; 32],
        value: T,
    ) -> Result<(), DurableError>;
}
```

`promise()` mints a `PromiseId` (UUIDv7) and a 32-byte CSPRNG resolver token. The token is stored
in `DurablePromise` (in-memory only, zeroized on drop). Its BLAKE3 hash is stored in
`durable_promises.resolver_token_hash`. On `resolve`, the token is constant-time compared against
the stored hash. No caller sees the raw token after `promise()` returns unless they hold the
`DurablePromise` value.

`await_promise` checks the journal on replay (→ already resolved → return). If unresolved:
`LocalBackend` uses `tokio::sync::Notify` keyed by `PromiseId` + a DB-poll fallback
(`promise_poll_interval_secs` default **2 s**, configurable). On `backend.open()` for a
resumed execution, ONE eager DB read for unresolved promises happens before entering the `Notify`
wait. Concurrent parked `Notify` is capped at `max_parked_promises` (default 1000); above the
cap, fall back to pure poll.

### DurableTimer (timer.rs)

```rust
impl DurableContext {
    async fn sleep_until(&self, due: SystemTime) -> Result<(), DurableError>;
}
```

On first execution: journals `TimerArmed`; parks in `DurableTimerService`.
On restart: armed-but-unfired timers are reloaded from the journal; timers whose `due_at` elapsed
during downtime fire immediately on recovery.

`DurableTimerService` is a background tokio task that polls `backend.due_timers(now_ms)` at
configurable intervals and wakes the parked `DurableContext`s.

---

## Persistence Schema

New tables added as numbered migration files in `zeph-db/migrations/sqlite/` and
`zeph-db/migrations/postgres/` (matching file counts, schema-equivalent — 031 §13). Applied via
`zeph_db::run_migrations(&durable_pool)` on the dedicated `durable.db` pool. All SQL uses the
`sql!()` macro for dialect-agnostic placeholder compatibility; the pool type is `zeph_db::DbPool`
from a `DatabaseDriver`-typed configuration — dual-backend correctness is preserved.

> **Pre-existing parity caveat (verified 2026-06-06, HEAD `b15adeb9`).** The `sqlite/` and
> `postgres/` migration sequences have already diverged: numbering drifts apart from `075` onward
> (the same logical migrations carry different numbers per dialect), and at least two migrations
> exist in only one dialect by name (`091_five_signal_retrieval.sql` is SQLite-only;
> `088_trajectory_memory_cascade.sql` is Postgres-only). File counts are SQLite=96 vs Postgres=95;
> the next unused SQLite number is `097`. C1 implementors MUST audit the Postgres sequence and
> assign the four new `durable_*` files so both dialects end with matching counts and
> schema-equivalent content — do NOT blindly mirror `097–100` into Postgres without reconciling
> the existing gap. This divergence predates spec-064 and is tracked as a separate issue (#4957).

```sql
-- durable_executions: one row per execution
CREATE TABLE durable_executions (
    execution_id TEXT    PRIMARY KEY,
    kind         TEXT    NOT NULL,
    status       TEXT    NOT NULL CHECK(status IN ('running','completed','failed','aborted')),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    finalized_at INTEGER                   -- NULL until terminal; drives retention
);
CREATE INDEX idx_durable_exec_status_time ON durable_executions(status, finalized_at);

-- durable_journal: append-only entries
CREATE TABLE durable_journal (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,  -- pg: BIGSERIAL
    execution_id TEXT    NOT NULL REFERENCES durable_executions(execution_id),
    step_id      INTEGER NOT NULL,
    entry_kind   TEXT    NOT NULL,
    idem_key     BLOB,              -- IdempotencyKey (32B); NULL for non-step entries
    effect_class TEXT,
    payload      BLOB,              -- AEAD-sealed (nonce||ct||tag); NULL for control entries
    payload_version INTEGER,
    hmac         BLOB,              -- row-level HMAC for control entries on shared-DB/Restate
    created_at   INTEGER NOT NULL
);
CREATE INDEX idx_durable_journal_exec_step
    ON durable_journal(execution_id, step_id, seq);
-- Enforce at most one committed result per step:
CREATE UNIQUE INDEX idx_durable_journal_result
    ON durable_journal(execution_id, step_id)
    WHERE entry_kind = 'step_result';
-- Efficient exactly-once intent lookup:
CREATE INDEX idx_durable_journal_idem_key
    ON durable_journal(execution_id, idem_key)
    WHERE idem_key IS NOT NULL;

-- durable_promises: external-completion handles
CREATE TABLE durable_promises (
    promise_id           TEXT PRIMARY KEY,
    execution_id         TEXT NOT NULL REFERENCES durable_executions(execution_id),
    resolver_token_hash  BLOB NOT NULL,   -- BLAKE3 of the 32-byte resolver token
    resolved             INTEGER NOT NULL DEFAULT 0,
    payload              BLOB,
    created_at           INTEGER NOT NULL,
    resolved_at          INTEGER
);

-- durable_timers: durable wakes
CREATE TABLE durable_timers (
    timer_id     TEXT    PRIMARY KEY,
    execution_id TEXT    NOT NULL REFERENCES durable_executions(execution_id),
    due_at       INTEGER NOT NULL,
    fired        INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);
CREATE INDEX idx_durable_timers_due ON durable_timers(fired, due_at);
```

**Notes:**
- `BLOB` → `BYTEA` in Postgres; `AUTOINCREMENT` → `BIGSERIAL`; partial index syntax is
  compatible.
- The `idx_durable_journal_result` unique partial index makes a double-result a DB-level error
  (defense in depth; the `JournalWriter` also guards at the application level).
- `idem_key` index covers the "does this intent already exist?" pre-execution check for
  `ExactlyOnceGuarded` steps — O(log n), not O(n).
- Under Postgres: tables share the server, use the `durable_` prefix; MVCC removes the
  contention concern.

### Transactional boundaries

- Single-row appends: one `INSERT` per `JournalMsg`.
- `ExactlyOnceGuarded` sequence: `EffectIntent` INSERT (committed + ACKed) → external effect
  runs → `StepResult` INSERT (committed + ACKed). Each INSERT is a separate transaction; the
  external effect is outside any DB transaction boundary by nature.
- `finalize` + last result: use `zeph_db::begin_write()` (SQLite `BEGIN IMMEDIATE` / Postgres
  `SELECT FOR UPDATE`) for multi-row atomic transitions.

### Retention & Compaction

`RetentionPolicy` (config-driven):

| Field | Default | Meaning |
|-------|---------|---------|
| `ttl_completed_secs` | 604800 (7d) | Prune completed executions older than this |
| `ttl_failed_secs` | 2592000 (30d) | Prune failed/aborted executions older than this |
| `max_executions` | 10000 | LRU cap on stored executions |
| `max_journal_bytes` | 1073741824 (1 GiB) | Size cap; triggers LRU sweep |
| `prune_batch_size` | 500 | Rows deleted per transaction; yield between batches |
| `prune_interval_secs` | 3600 (1h) | Background task poll interval |
| `stale_running_after_secs` | 3600 (1h) | Crash-orphan threshold (#6254): a `status='running'` row whose `updated_at` is older than this becomes a sweep candidate. Default 1h is generous enough that no genuinely-active long turn is ever a candidate before the flock check even runs, yet small enough that orphans re-enter retention within the hour. `0` disables the sweep. See Crash-Orphan Sweep below. |

Background pruning NEVER runs on the dispatch/append hot path. A background tokio task runs
`prune()` every `prune_interval_secs`. Pruning deletes in batches of `prune_batch_size` rows per
transaction, releases the lock, yields, and loops — no large-transaction stall.

#### Crash-Orphan Sweep (#6254)

**Requirement.** An execution whose owner process exits ungracefully (SIGKILL, panic, OOM,
power-loss) leaves its `durable_executions` row `status='running', finalized_at=NULL` forever — no
graceful detach path runs, so the row is invisible to the TTL prune above (which only ever
considers `finalized_at IS NOT NULL` rows) permanently. The sweep gives such crash-orphaned rows a
terminal status so they re-enter the normal retention lifecycle.

**Mechanism.** The same background task that runs `prune()` (the existing supervised retention
loop — no new `TaskSupervisor` spawn site) calls `Journal::sweep_orphans(&policy)` **before**
`prune()` on every tick:

```
tick:
    backend.sweep_orphans(&policy).await   // NEW — must run before prune()
    backend.prune(&policy).await           // existing
```

`sweep_orphans` (see INV-17 for the full invariant):
1. If `policy.stale_running_after_secs == 0`, return `Ok(0)` (disabled).
2. If the backend has no `lock_dir` (`:memory:`, Postgres, non-Unix), warn-once and return `Ok(0)`
   — see Non-Goals and NEVER. This is a documented no-op, not a silent gap.
3. Batch-scan (`prune_batch_size` rows per batch, yielding between batches — same discipline as
   `prune`) `status='running' AND updated_at <= now - stale_running_after_secs`.
4. For each candidate, non-blocking try-acquire its INV-15 `ExecutionLock`:
   - `ExecutionLocked` (lock held) → live owner → skip.
   - Acquired → no live owner → while still holding the lock, run
     `UPDATE durable_executions SET status='aborted', finalized_at=now, updated_at=now WHERE
     execution_id=? AND status='running' AND finalized_at IS NULL`, then release the lock.
5. Return the count aborted.

The sweep never deletes rows and never retries/resumes an orphan — it only finalizes. Deletion
stays exclusively with `prune()`; resume stays exclusively with the pre-existing
`ensure_session_durable_ctx` → `open_execution` replay path on the next legitimate open (INV-17).

**Dispatch & Postgres compile-correctness (M1/M2).** `sweep_orphans` is a `Journal` trait method
(see Journal trait above), dispatched through `DurableBackendEnum` — not called on `LocalBackend`
directly. The new SQL (the candidate `SELECT` and the guarded abort `UPDATE`) MUST go through the
project's `sql!()` macro so both queries compile and rewrite placeholders correctly under
`--features postgres`, even though `sweep_orphans` is a `lock_dir=None` no-op on Postgres at
runtime — the code path still has to compile there.

**Ordering (M3 tick-safety).** The sweep MUST run before the same tick's `prune()` so a
just-aborted orphan is visible to that tick's TTL check. With `ttl_failed_secs=0`, a freshly-swept
orphan (`finalized_at = now`) becomes prune-eligible on that same tick (cutoff `now - 0 = now`,
`finalized_at <= now` matches) — intended and harmless, since `0` already means "prune
failed/aborted immediately."

**Zero DB migrations.** `status`, `updated_at`, and `finalized_at` already exist on
`durable_executions`; `'aborted'` is already a legal `CHECK` value. No new column, no new index —
the existing `idx_durable_exec_status_time (status, finalized_at)` index already restricts the scan
to `status='running'`; filtering the (small, bounded-by-live-concurrency) result set on `updated_at`
in memory is cheap.

**Observability.** Span `durable.retention.sweep_orphans` with an `aborted_count` attribute
(mirrors `durable.journal.prune`), and a metric `durable.retention.orphans_aborted` — see Tracing
Spans.

**In-execution step cap:** `max_steps_per_execution` (default **10000**). On soft exceed
(90% of cap): the `JournalWriter` forces a `Checkpoint` fold of the committed-idempotent prefix
below the current replay point. On hard exceed: the execution is aborted with
`DurableError::StepCapExceeded` rather than allowed to grow unboundedly. The background prune only
touches terminal executions; the cap handles long in-flight sessions.

**Checkpoint fold:** a `CheckpointEntry { up_to_step, snapshot: Bytes }` replaces all preceding
`Idempotent` `StepResult` entries that have no pending promise or timer reference. Aggressive
compaction beyond this fold is deferred to post-v1.

---

## Integration Adapters

### P1 — Agent Tool Loop (`zeph-core`, `src/agent/tool_execution/tier_loop.rs`)

This integration is an **explicit rewrite of the agent-loop control flow**, not a hook. A
`&mut DurableContext` (or `Arc<DurableContext>` for `&self`) is threaded through the loop in
`zeph-core` (`src/agent/tool_execution/tier_loop.rs`). The LLM call and each tool dispatch are wrapped as
`ctx.step(...)`. Concretely:

- LLM call: `ExactlyOnceGuarded`, `CostBearingOrBoundaryIdempotent`, `on_ambiguous: Skip`.
  Payload stores a `MessageId` reference to the already-persisted `messages` row, NOT the full
  `ChatResponse` bytes (avoids doubling WAL pressure; resolves on replay by rehydrating from
  `messages` by `MessageId`).
- Tool dispatch: `EffectClass` determined by tool metadata (destructive tools → `ExactlyOnce
  Guarded` + `Destructive`; read-only tools → `Idempotent`; queue-injection tools → `AtLeastOnce`).
- On crash mid-turn: the next startup opens the `ExecutionId` from the in-progress session, builds
  a `ReplayCursor`, and steps through the journal — replaying journaled steps without re-invoking
  `op`, resuming at the first un-journaled step. The discard-and-sanitize behavior of
  `sanitize_tool_pairs()` becomes a resume.
- `StepOutcome::Replayed` discriminator is surfaced through `RuntimeLayer` for the single purpose
  of suppressing user-visible re-emission of already-printed assistant output. Replay *control*
  does not flow through `RuntimeLayer`.
- `ExecutionId` derivation (`Agent::ensure_session_durable_ctx`,
  `crates/zeph-core/src/agent/durable_bootstrap.rs`) folds `ConversationId` and `sqlite_path` — two
  processes agreeing on both derive the same id. `ensure_session_durable_ctx` opens it via
  `LocalBackend::open_execution_exclusive` (INV-15), not the plain `open_execution`, so a second
  concurrent process on the same conversation degrades to non-durable instead of corrupting the
  first process's journal (#6122).

**LLM-serialization gate (MANDATORY for P1 PR).** The P1 implementation touches the LLM call
serialization path (`zeph-core/src/agent/tool_execution/tier_loop.rs`). Before the PR is merged:
1. Run a live API session test: multi-turn prompt + at least one tool call.
2. Verify: no 400/422 errors in log, debug dump shows a well-formed `messages` array,
   LLM returns a coherent response.
3. Document test result in the PR description.
This matches the LLM serialization gate defined in `branching.md`.

### P2 — Orchestration `/plan resume` (`zeph-orchestration`)

Thin adapter in `zeph-orchestration/src/durable.rs`. Scope: the `/plan resume <id>` user command
path (`zeph-core/src/agent/plan.rs`: `handle_plan_resume_as_string` defined at L1006, dispatched
at L1161 → `resume_loaded_graph` at L935 → `DagScheduler::resume_from` in
`zeph-orchestration/src/scheduler/mod.rs:380`).

**What the journal carries that resume_from resets today:**
`task_replan_counts`, `global_replan_count`, `predicate_replans_used`, `predicate_reasons`,
`lineage_chains` (zeroed by `scheduler/mod.rs:543-559` today). The durable journal carries these
counters so that `resume_from` restores them instead of zeroing — a real correctness fix on the
existing reachable resume path.

**What the journal does NOT carry:** `pending_permits` (`OwnedSemaphorePermit`). These are
non-serializable and are reconstructed lazily by the existing `RunningTask::admission_permit: None`
re-acquisition on next `tick()` dispatch (`scheduler/mod.rs:485-487, 562-567`). The journal
carries the `running` set (which tasks were in-flight); permits are re-acquired by the existing
mechanism. This is not a new contribution — it documents existing behavior.

**Scope honesty:** P2 does NOT fix automatic crash-recovery on process start. There is no
auto-reload-and-resume on startup today; P2 does not add it. A future epic may wire that path;
the durable journal provides the substrate.

**Flock-liveness conversion (#6254, m1/m3).** `journal_budget`'s `DagRun` execution-open call
converts from plain `open_execution` to `open_execution_exclusive`, holding the returned
`ExecutionLock` as a local for the function's lifetime — this is what makes the row observable to
the INV-17 crash-orphan sweep (its `budget_exec_id(graph_id, generation)` id is externally-derived
from a monotonic save-generation, so exclusive is the structurally-correct call regardless of the
sweep; see INV-15/#6122). `journal_budget` MUST keep passing `is_resume=false` into
`build_budget_ctx` exactly as before — lock acquisition and the resume flag are orthogonal;
converting the open call does not change the "always a fresh generation, never a resume" semantics
of this path. On `DurableError::ExecutionLocked` (another live instance already owns this
`graph_id`+`generation`), `journal_budget` MUST log and return `Ok(())` — skip journaling this
budget snapshot, do not retry, do not surface an error. This is benign: the existing caller
(`plan.rs:369`) already tolerates a missing snapshot (budget zeroes on next resume). A test MUST
assert `ExecutionLocked` → `Ok(())`, not an error.

### P3 — Scheduler Exactly-Once (`zeph-scheduler`)

Thin adapter in `zeph-scheduler` wrapping `JobStore.record_run()`. Each job fire opens an
`ExecutionKind::ScheduledJob` execution. The `IdempotencyKey` is derived from `(job_name,
scheduled_fire_time_ms)`.

**Protocol:**
- Before job invocation: `AppendBuffered` `EffectIntent` for `AtLeastOnce` jobs (most prompt-
  injection jobs); `AppendAcked` `EffectIntent` for jobs explicitly marked `ExactlyOnceGuarded`.
- After job completes: `AppendBuffered`/`AppendAcked` `StepResult`.
- On crash recovery: intent-present + result-absent → `OnAmbiguous` per job class.

Respects the invariant "fire via `message_queue` injection, never direct agent call."

**Flock-liveness conversion (#6254, m1).** `fire_with_durable`'s `ScheduledJob` execution-open
call converts from plain `open_execution` to `open_execution_exclusive`, holding the returned
`ExecutionLock` as a local for the entire fire-body function (open → step → finalize) — this is
what makes a slow-but-alive job (e.g. a multi-hour nightly job body, held as one `ctx.step`)
correctly skip the INV-17 crash-orphan sweep instead of being false-aborted. Its id
(`derive_execution_id(job_name, slot_ms)`) is externally-derivable, so exclusive is the
structurally-correct call independent of the sweep — it also closes a latent double-drive gap
where two scheduler daemons could otherwise both fire the same `job_name`+`slot_ms` (INV-15/#6122).
On `DurableError::ExecutionLocked` (a peer daemon already owns this slot), the handler MUST log at
info/debug and return `Ok(())` — skip this fire, do not retry (a retry risks a duplicate; the peer
is already firing it), and MUST NOT map to `SchedulerError::TaskFailed`. A test MUST assert
`ExecutionLocked` → `Ok(())`, not a task failure.

### P4 — Subagent Durable Promise (`zeph-subagent`)

Parent opens a `DurablePromise<SubagentResult>` at spawn time; the subagent resolves it on
completion via the `DurableHandle` resolver. On parent crash: restarted execution replays to the
journaled resolution if the child finished (via a non-blocking check,
`try_replay_durable_subagent` in `zeph-subagent`, wrapping `DurableContext::take_resolved_promise`)
before spawning a fresh child. If the promise is still pending, the general re-park behavior
described above applies to distributed backends; the v1 `LocalBackend`-only integration
(`resolve_durable_spawn_gate` in `zeph-core`, #5944) falls back to a plain spawn instead of
parking — subagents run as in-process tokio tasks, so a still-pending promise after a real
process crash always means the child died with the parent (never "genuinely still running"),
and INV-9 makes its resolver token unrecoverable either way, so parking would hang forever.

The JSONL transcript remains the human-readable record; the durable journal carries resumable
control state. Reconciles with spec-063 (worktree teardown) and spec-044 (MCP re-inherit on
respawn) — durable resume reuses the existing respawn path.

---

## The Three Hard Problems

### 1. Replay determinism vs Zeph's native parallel tool execution

Restate enforces single-threaded deterministic replay; Zeph runs tool calls concurrently via
`join_all` in `zeph-core/src/agent/tool_execution/tier_loop.rs` (`execute_tier_join`, L1705). Naive journaling would assign StepIds in
completion order, racing under concurrency.

**Resolution (INV-2):** StepId is assigned at call time (structural), not at completion
(temporal). Concurrent steps have stable, replay-stable ids regardless of which finishes first.
The `ReplayCursor` looks up by `StepId`; out-of-order completion is irrelevant.

`parallel()` reserves a contiguous `[base..base+n)` block upfront (via `AtomicU32::fetch_add(n)`)
so child ids are deterministic in argument order. `BackendCapabilities::parallel_steps = false`
(RestateBackend) collapses `parallel()` to sequential durable wrapping — the journal is sequential
but underlying native tool I/O still runs concurrently (the durability wrapper does not serialize
the I/O, only the recording).

Non-determinism sources (time, random, provider responses) MUST be captured as `DurableStep`
results: e.g. `ctx.step("now", Idempotent, ...)` journals the timestamp so replay sees the
original. This is a documented contract enforced by code review. A clippy lint / `#[durable]`
macro is post-v1 defense-in-depth.

### 2. Exactly-once vs at-least-once for non-idempotent side effects

Covered by the `EffectClass` contract, `OnAmbiguous` per-intent default, the
`EffectIntent`/`StepResult` pairing in the journal, and the `IdempotencyKey` for boundary dedup.

**Honesty contract:** exactly-once across an un-cooperating external boundary is impossible. This
design provides exactly-once *within the journal* and best-effort dedup *at the boundary* via the
`IdempotencyKey` (forwarded as an `Idempotency-Key` header where the external service supports
it). The contract is stated plainly in API documentation; it is not oversold.

Paid LLM calls: `ExactlyOnceGuarded`, `OnAmbiguous::Skip`. A crash after the API returns but
before journaling means we skip the retry (cost already incurred; never double-charged).

### 3. Journal growth / compaction / retention

Covered by `RetentionPolicy`, the background prune task, the `max_steps_per_execution` cap, and
the `Checkpoint` fold. Long in-flight agent sessions are bounded by the step cap; terminal
executions are cleaned by TTL. Hot-path is never blocked by pruning (INV).

---

## Config Schema (`[durable]`)

```toml
[durable]
enabled = false                     # opt-in; false = current behavior, no journal opened
backend = "local"                   # "local" | "restate"
encrypt_payload = true              # false = dev override; WARN at startup; FORBIDDEN for non-local
agent_turns = true                  # P1: wrap agent loop steps
orchestration = true                # P2: /plan resume replan-budget journaling
scheduler = true                    # P3: scheduler exactly-once
subagent = true                     # P4: subagent durable promise

journal_flush_interval_ms = 10      # group-commit interval for AppendBuffered
journal_ack_timeout_ms = 5000       # AppendAcked timeout; → JournalUnavailable
max_steps_per_execution = 10000     # in-execution step cap; soft = 90%, hard = 100%
max_payload_bytes = 1048576         # 1 MiB default; enforced on both read and write

promise_poll_interval_secs = 2      # DB fallback poll for parked promises
max_parked_promises = 1000          # above cap: fallback to pure poll

[durable.retention]
ttl_completed_secs = 604800         # 7 days
ttl_failed_secs = 2592000           # 30 days
max_executions = 10000
max_journal_bytes = 1073741824      # 1 GiB
prune_batch_size = 500
prune_interval_secs = 3600
stale_running_after_secs = 3600     # crash-orphan threshold (#6254); 0 disables the sweep

# RestateBackend sub-table (only meaningful when backend = "restate" + feature = "restate")
[durable.restate]
# ingress_url → vault key ZEPH_RESTATE_INGRESS_URL (never inline here)
# api_key     → vault key ZEPH_RESTATE_API_KEY
allow_insecure = false              # FORBIDDEN to set true in production
```

All fields use `#[serde(default)]`. No provider credentials appear inline (spec-038 vault
contract).

**New vault keys (registered in vault at first run / `--init`):**
- `ZEPH_DURABLE_KEY` — raw key bytes for `PayloadCipher` (XChaCha20-Poly1305).
- `ZEPH_RESTATE_INGRESS_URL` — Restate ingress URL (only with `restate` feature).
- `ZEPH_RESTATE_API_KEY` — Restate API key if required (only with `restate` feature).

---

## Mandatory Integration Points

### 1. CLI — `zeph durable`

Analogous to `zeph schedule`. Connects directly to `durable.db`; no agent process required.

| Subcommand | Description |
|-----------|-------------|
| `zeph durable list [--status <s>] [--kind <k>]` | List executions with id, kind, status, created_at, step count |
| `zeph durable show <execution_id>` | Show journal entries (metadata only by default; payload redacted) |
| `zeph durable show <execution_id> --reveal` | Show with decrypted payload (WARNING printed) |
| `zeph durable inspect <execution_id> --step <n>` | Inspect a single step entry |
| `zeph durable prune [--dry-run]` | Force crash-orphan sweep, then TTL prune (#6254). `--dry-run` reports both counts separately: "N orphaned executions would be aborted" and "M would be pruned" |
| `zeph durable resume <execution_id>` | Manual replay trigger (for supported execution kinds) |

**Redaction rule (INV-5):** default output shows only: `entry_kind`, `step_id`, `effect_class`,
payload size in bytes, `idem_key` (hex, first 8 bytes), `created_at`. Payload bytes and resolver
tokens are never shown without `--reveal`.

### 2. TUI — `DurableView` and spinners

- Command palette entry: `durable` → opens `DurableView`.
- `DurableView`: toggle-able panel (e.g. `D` key) showing in-flight executions with
  `execution_id` (short form), `kind`, `status`, current `step_id`, elapsed time.
- Per TUI rules (spec-011): every background durable operation MUST have a visible spinner:
  - `Replaying execution…` (replay in progress)
  - `Pruning journal…` (background prune)
  - `Awaiting external completion…` (promise parked)
  - `Journal unavailable — non-durable mode` (ACK timeout degradation)

**Crash-orphan sweep (#6254): minimal/no new TUI surface.** The sweep runs on the same background
loop as `prune()`, which has no mandatory palette command today (only the optional `Pruning
journal…` status line above). No new TUI command or palette entry is added for the sweep. If/when
the retention task's status label is extended, it MAY read `Sweeping orphaned executions…` before
`Pruning journal…`; this is optional polish, not a requirement.

### 3. `--init` Wizard

Step in the interactive configuration wizard offering:
1. Enable durable execution? (y/n, default n)
2. Backend: `local` (default) | `restate`
3. Retention defaults (accept defaults or customize TTL/size). `stale_running_after_secs` (#6254)
   is not individually surfaced here, consistent with the other `RetentionPolicy` fields — accepting
   defaults includes it.
4. (If backend = restate) Vault key configuration for `ZEPH_RESTATE_INGRESS_URL` and
   `ZEPH_RESTATE_API_KEY`.
5. Generate `ZEPH_DURABLE_KEY` and store in age vault.

### 4. `--migrate-config`

Migration step adds `[durable]` section with all defaults to existing configs. The migration is
purely additive and default-off (`enabled = false`), so no behavior change on upgrade. Migration
step is idempotent (skip if `[durable]` already present).

**`stale_running_after_secs` field injection (#6254).** Two migration paths:
1. Fresh migration (no `[durable]` present): the commented `[durable.retention]` block includes
   `# stale_running_after_secs = 3600` alongside the other retention defaults.
2. Existing, uncommented `[durable.retention]` table that predates #6254 and lacks the field: a
   targeted field-injection step inserts `stale_running_after_secs = 3600` into it (path 1's
   early-return on "`[durable]` already present" would otherwise skip this config forever). Because
   the field is `#[serde(default)]`, a config that never runs this migration step still loads with
   the correct default at runtime — the migration is a self-documentation convenience, not a
   correctness requirement.

### 5. Testing Playbook

File: `/Users/rabax/Dev/zeph/.local/testing/playbooks/durable-execution.md`

Must cover:
1. **Crash-resume of agent turn** — kill process mid-turn, restart, verify resumption from last
   journaled step; verify assistant output not double-printed.
2. **DAG resume with preserved replan budget** — `/plan resume` after plan exhausted replans;
   verify replan counters restored (not zeroed).
3. **Scheduler exactly-once** — simulate double-fire (kill between intent and result); verify the
   guarded job does not re-execute on restart.
4. **Subagent orphan resume** — kill parent mid-subagent wait; restart; verify
   `await_promise` returns the journaled result if child finished.
5. **Retention sweep** — inject old completed executions; force prune; verify removal.
6. **Ambiguous-window handling** — simulate crash after effect, before StepResult; verify
   `OnAmbiguous` policy is applied and audit record emitted.
7. **Parallel-step replay determinism** — run a parallel tool batch, kill mid-batch, resume;
   verify all step ids replay deterministically.
8. **ReplayDivergence guard** — manually corrupt a journal fingerprint; verify
   `DurableError::ReplayDivergence` is raised and fresh restart occurs.
9. **Promise resolution auth** — attempt resolution with wrong resolver token; verify rejection.
10. **Key rotation** — rotate `ZEPH_DURABLE_KEY`; verify in-flight executions complete or drain
    cleanly.
11. **Crash-orphan sweep (#6254)** — `kill -9` an agent process mid-turn; verify the
    `durable_executions` row stays `status='running', finalized_at=NULL` (invisible to plain TTL
    prune). Run `zeph durable prune` (or wait a tick with `stale_running_after_secs` lowered for
    the test); verify the row flips to `status='aborted', finalized_at=<set>`. Separately, start a
    live session and force a sweep tick while it is idle-but-alive (holding its
    `ExecutionLock`); verify its row is NOT swept (still `running`, not aborted). Verify
    `--dry-run` reports the orphan count without mutating state.
12. **`ExecutionLocked` graceful skip (#6254, m1)** — for both the scheduler `ScheduledJob` and
    orchestration `DagRun` adapters: with two processes contending for the same execution id
    (same job+slot, or same graph+generation), verify the loser receives `DurableError::
    ExecutionLocked` and the adapter returns `Ok(())` — no `SchedulerError::TaskFailed`, no error
    surfaced, no retry.
13. **Reopen unifies `aborted` (#6254, INV-16)** — abort an execution (via the sweep or via
    divergence-recovery), then reopen its `ExecutionId` (e.g. resume the same `ConversationId`);
    verify the row resets to `status='running', finalized_at=NULL` rather than staying finalized
    and prune-eligible.

### 6. Coverage-Status Rows

Add to `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` with status `Untested`:

| Component | Playbook scenario |
|-----------|------------------|
| `zeph-durable` journal (LocalBackend) | 1, 5, 8 |
| `DurableStep` replay — Idempotent/AtLeastOnce | 1, 7 |
| `DurableStep` replay — ExactlyOnceGuarded | 3, 6 |
| Idempotency / exactly-once protocol | 3, 6 |
| Durable promises (create / await / resolve) | 4, 9 |
| Durable timers | 1 (verify timer re-arm) |
| JournalWriter actor (group-commit, ACK, restart) | 1, 3 |
| ReplayDivergence guard | 8 |
| PayloadCipher AEAD (seal/open/rotation) | 10 |
| P1 agent-loop integration | 1 |
| P2 orchestration `/plan resume` | 2 |
| P3 scheduler exactly-once | 3 |
| P4 subagent durable promise | 4 |
| Retention sweep | 5 |
| Crash-orphan sweep (#6254) | 11, 13 |
| Scheduler/orchestration `ExecutionLocked` graceful skip (#6254) | 12 |
| `zeph durable` CLI | 1, 5, 11 |
| TUI `DurableView` + spinners | 1 |

---

## Feature Flags

- **`zeph-durable` core: ALWAYS-ON (no flag).** `LocalBackend` gates no optional dep beyond what
  `zeph-db` already provides (`blake3`, `chacha20poly1305`, `serde`, `tokio`, `tracing` are all
  workspace-present). Per spec-029 Decision Rule §2, a flag is justified only if it gates a real
  optional dep. The in-process layer does not; adding a flag would be a forbidden "pure behavioral
  marker." Runtime opt-in is via `[durable] enabled = false` (default).

- **`restate` (NEW, justified):** gates `dep:restate-sdk`. Assigned to the `server` bundle
  (cloud/multi-process). NOT added to `full` — Restate requires an external server, analogous to
  `postgres`, `metal`, `cuda`. It gets its own CI lane. The `cargo deny` + license gate is
  MANDATORY before this feature merges.

- **No new `*_provider` field.** `zeph-durable` makes no LLM calls. The `IdempotencyKey` MAY be
  forwarded to the provider as an `Idempotency-Key` header (consumer-adapter responsibility), but
  this is a consumer-side wiring detail. No new provider config is introduced (correct per
  spec-024 — durable execution is not an LLM subsystem). The `ExactlyOnceGuarded` journal step
  for an LLM call does forward the key to the provider for boundary-dedup, but the provider
  selection is entirely the agent-loop's (already resolved via the provider registry).

---

## Tracing Spans

Span naming convention: `<crate_short>.<subsystem>.<operation>`.

| Span | Attributes | Notes |
|------|-----------|-------|
| `durable.journal.append` | `execution_id`, `step_id`, `entry_kind` | Per append |
| `durable.journal.read` | `execution_id`, `step_count` | Full read |
| `durable.journal.read_segment` | `execution_id`, `from_step_id`, `count` | Range read (replaces full for long sessions) |
| `durable.journal.prune` | `deleted_count` | Background sweep |
| `durable.retention.sweep_orphans` | `aborted_count` | Crash-orphan sweep (#6254); runs immediately before `durable.journal.prune` each tick. Metric: `durable.retention.orphans_aborted`. |
| `durable.journal.writer.queue_depth` | gauge value | Gauge event per commit cycle |
| `durable.step.run` | `step_id`, `effect_class`, `replayed: bool` | Per step; `replayed` drives perf regression gate |
| `durable.step.replay` | `step_id`, `effect_class` | Replay path only |
| `durable.promise.create` | `promise_id` | |
| `durable.promise.await` | `promise_id` | |
| `durable.promise.resolve` | `promise_id` | Hash only in attrs, never raw token |
| `durable.timer.arm` | `timer_id`, `due_at_ms` | |
| `durable.timer.fire` | `timer_id` | |
| `durable.backend.open` | `execution_id`, `kind`, `is_resume: bool` | |
| `durable.replay.cursor.build` | `execution_id`, `step_count` | Cursor construction |
| `durable.replay.cursor.read_segment` | `from_step_id`, `count` | Measures range-read latency |
| `core.durable.turn` | `execution_id`, `turn_number` | P1 agent-loop adapter |
| `orch.durable.dag` | `execution_id`, `task_id` | P2 orchestration adapter |
| `sched.durable.fire` | `execution_id`, `job_name` | P3 scheduler adapter |
| `subagent.durable.await` | `execution_id`, `promise_id` | P4 subagent adapter |

Every `async fn` in `zeph-durable` that awaits an external resource (DB write, promise park,
timer poll) MUST carry a tracing span. Uninstrumented code is invisible to the CI trace analysis
loop (spec-continuous-improvement.md instrumentation requirement).

---

## Multi-Model Design

`zeph-durable` makes no LLM calls and introduces no new `*_provider` config field. Provider
selection for wrapped LLM steps stays entirely in the consumer (already resolved via the provider
registry per spec-023/spec-024). The durable wrapper journals opaque `MessageId`-referenced
results without knowing which provider produced them.

One cross-cutting note for consumer adapters: when wrapping an LLM call as `ExactlyOnceGuarded`,
the consumer SHOULD forward the `IdempotencyKey` to the provider call as an `Idempotency-Key`
header (where the provider supports it) for best-effort boundary dedup. This is consumer-side
wiring; `zeph-durable` provides the key via `StepHandle`, not a config field.

---

## Reconciliation with Existing Specs

| Spec | Relevant invariant | How this spec reconciles |
|------|--------------------|--------------------------|
| **001** §10 concurrency | Single-threaded async; concurrent tasks, not parallel OS threads | `DurableContext` is `&self` + `AtomicU32`; concurrent `step()` calls are safe; `parallel()` uses `fetch_add(n)` for contiguous reserved blocks. |
| **001** §13 DB backend | SQLite/Postgres parity; `zeph_db::DbPool`; all SQL through `sql!()` macro | Dedicated `durable.db` pool using `DatabaseDriver`/`DbPool`/`sql!()`; durable schema files in `zeph-db/migrations/` applied via `zeph_db::run_migrations`; Postgres variant uses same types. |
| **001** §15 RuntimeLayer | `&self` hooks, non-fatal, observation-only | RuntimeLayer receives `StepOutcome::Replayed` to suppress double-print. No replay *control* flows through it. |
| **009** orchestration | `GraphPersistence::save()` after every transition; `DagScheduler::resume_from` | P2 adds a parallel journal; `resume_from` restores replan counters from journal instead of zeroing. `pending_permits` use existing lazy re-acquisition. #6254 converts `journal_budget`'s execution-open to `open_execution_exclusive`, making `DagRun` liveness observable to the crash-orphan sweep (INV-17); `ExecutionLocked` degrades to a graceful `Ok(())` skip, never a task failure. |
| **018** scheduler | `JobStore.record_run()` sole persistence path | P3 wraps `record_run()` as a `DurableStep`; `JobStore` retains sole ownership of `scheduled_jobs`. #6254 converts `fire_with_durable`'s execution-open to `open_execution_exclusive`, making `ScheduledJob` liveness observable to the crash-orphan sweep (INV-17) and closing a latent cross-daemon double-fire gap; `ExecutionLocked` degrades to a graceful `Ok(())` skip, never `SchedulerError::TaskFailed`. |
| **029** feature flags | Flags gate real optional deps; no behavioral markers | `restate` flag gates `dep:restate-sdk`. Core has no flag. |
| **031** database abstraction | Single migration runner (`sqlx::migrate!` only in `zeph-db`); `DbPool` from `DatabaseDriver` | Dedicated `durable.db` pool (own `DbConfig::connect()` — valid precedent from `JobStore`). Schema files added to `zeph-db/migrations/{sqlite,postgres}/`; applied via `zeph_db::run_migrations(&durable_pool)`. `zeph-durable` owns NO `.sql` files and NO `sqlx::migrate!` — single source of truth preserved (031 §12). |
| **038** vault | All secrets vault-resolved; `ZEPH_*` keys | `ZEPH_DURABLE_KEY`, `ZEPH_RESTATE_*` are vault-resolved; never inline TOML. |
| **039** background-task-supervisor | Tracked via `TaskSupervisor`; supervised restart | `JournalWriter` tokio task is tracked via `zeph-common::TaskSupervisor` (the unified `JoinSet` wrapper, spec-039) under the daemon supervisor; on panic, supervisor restarts the writer, which re-reads the last committed `JournalSeq` and resumes (INV-12, FR-DE-12). #6254's crash-orphan sweep introduces NO new spawn site: it folds into the already-supervised retention loop (`sweep_orphans()` called before `prune()` each tick), inheriting that loop's existing `TaskSupervisor::spawn` restart policy at both existing spawn call sites. |
| **044** subagent lifecycle | Transcript JSONL + `.meta.json` remains the human record | P4 adds a durable promise for control state; transcript unchanged. |
| **057** agent persistence | `NEVER double-persist`; `sanitize_tool_pairs` discards orphans | P1 replays journaled steps (no re-insert). `Idempotent` step replay skips `op`. The discard becomes a resume (INV-10). |
| **063** worktree subsystem | Subagent spawning, cwd isolation | P4 durable resume reuses the existing respawn path; CwdGuard discipline is unaffected. |
| **068** session persistence | Mirrors the `zeph-durable` append-only journal, bounded-buffer replay cursor, and single-writer actor — but at the conversation/context level, not the task/step level. `zeph-session` MUST NOT depend on `zeph-durable` (INV-1: no agent types). The two journals are independent and reference each other only by opaque IDs. If shared primitives are extracted, they belong in `zeph-common`. See `specs/068-session-persistence/spec.md §3` and `§15` (NEVER). |

---

## Benchmarks & Acceptance Criteria

### Functional Requirements

| ID | Requirement |
|----|------------|
| FR-DE-01 | When `durable.enabled = true` and a `DurableContext` is open, every call to `ctx.step()` MUST record the step in the journal before `op` returns on first execution. |
| FR-DE-02 | On replay, `ctx.step()` MUST return the journaled result without invoking `op` when the journal contains a matching `StepResult` for the current `StepId`. |
| FR-DE-03 | On `ReplayDivergence` (fingerprint mismatch at any StepId), the execution MUST be discarded and restarted fresh. The partial journal MUST be marked `aborted`. |
| FR-DE-04 | `ExactlyOnceGuarded` step: `EffectIntent` MUST be committed (ACKed) before `op` is invoked. `StepResult` MUST be committed (ACKed) after `op` returns. |
| FR-DE-05 | A `DurablePromise` created by `promise()` MUST be resolvable only by presenting the matching resolver token via `DurableHandle::resolve()`. Incorrect tokens are rejected with constant-time comparison. |
| FR-DE-06 | A `sleep_until` whose `due_at` elapsed during downtime MUST fire immediately on the first `DurableTimerService` poll after restart. |
| FR-DE-07 | `zeph durable show <id>` MUST NOT include payload bytes in its default output. `--reveal` MUST display a warning before showing decrypted content. |
| FR-DE-08 | `zeph durable list` MUST work against a `durable.db` file while no agent process is running. |
| FR-DE-09 | A destructive `ExactlyOnceGuarded` step without an explicit `on_ambiguous` field MUST fail with `DurableError::AmbiguityPolicyRequired` at `StepDescriptor` construction time. |
| FR-DE-10 | Every ambiguous-window resolution MUST emit a structured audit record (tracing WARN + audit sink) with `execution_id`, `step_id`, `effect_class`, `idem_key`, `on_ambiguous` value. |
| FR-DE-11 | `AppendAcked` MUST return `DurableError::JournalUnavailable` after `journal_ack_timeout_ms` if the writer has not ACKed. The calling task MUST NOT block indefinitely. |
| FR-DE-12 | On `JournalWriter` restart, it MUST query `MAX(seq)` from `durable_journal` and resume from the last committed `JournalSeq`. |
| FR-DE-13 | P2: `/plan resume <id>` MUST restore `task_replan_counts`, `global_replan_count`, `predicate_replans_used`, `predicate_reasons`, and `lineage_chains` from the journal. |
| FR-DE-14 | P3: a scheduler job fire whose `EffectIntent` is journaled but `StepResult` is absent on restart MUST apply the job's configured `OnAmbiguous` policy; it MUST NOT unconditionally re-fire. |
| FR-DE-15 | Payload encryption MUST use XChaCha20-Poly1305 with a fresh random 24-byte nonce per `seal`. Stored layout: `nonce(24B) \|\| ciphertext \|\| tag(16B)`. |
| FR-DE-16 | (#6254) The crash-orphan sweep MUST abort a `status='running'` row with `updated_at <= now - stale_running_after_secs` ONLY after a non-blocking try-acquire of that execution's `ExecutionLock` succeeds; `ExecutionLocked` MUST short-circuit to skip (no abort). |
| FR-DE-17 | (#6254) The sweep's abort `UPDATE` MUST run while still holding the acquired `ExecutionLock`, guarded by `WHERE execution_id=? AND status='running' AND finalized_at IS NULL`, and MUST run before that tick's `prune()` call. |
| FR-DE-18 | (#6254) `stale_running_after_secs = 0` MUST disable the sweep entirely (`sweep_orphans` returns `Ok(0)` without scanning). |
| FR-DE-19 | (#6254) On a backend with `lock_dir=None` (`:memory:`, Postgres, non-Unix), `sweep_orphans` MUST return `Ok(0)` and emit a warn-once log; it MUST NOT abort any row on staleness alone. |
| FR-DE-20 | (#6254) `zeph-scheduler`'s `fire_with_durable` and `zeph-orchestration`'s `journal_budget` MUST open their execution via `open_execution_exclusive`; on `DurableError::ExecutionLocked` both MUST return `Ok(())` without retry and without surfacing a task-failure error. |
| FR-DE-21 | (#6254) `open_execution`/`open_execution_exclusive`'s reopen path MUST reset `status='running'` and clear `finalized_at` for a row in ANY of `completed`, `failed`, or `aborted` — not only `completed`/`failed`. |

### Non-Functional Requirements (measurable)

| ID | Requirement | Method |
|----|------------|--------|
| NFR-DE-01 | `bench_step_run_exactly_once_n` at N=5 steps: p99 ≤ 5 ms (criterion; CI gate) | per-crate `crates/zeph-durable/benches/` criterion harness (workspace convention; not `zeph-bench`) |
| NFR-DE-02 | Resume read latency for a 5000-step execution using range cursor: ≤ 5 ms total (50 × 100-entry segments × 0.1 ms) | Integration test with synthetic journal |
| NFR-DE-03 | `AppendBuffered` group-commit overhead: ≤ 1 µs per `send()` on an un-filled channel | Criterion microbench |
| NFR-DE-04 | `AppendAcked` round-trip latency (writer WAL commit): ≤ 3 ms p99 on local SQLite WAL-Normal | Criterion bench |
| NFR-DE-05 | Max payload enforced: payloads > 1 MiB return `DurableError::PayloadTooLarge` in < 1 µs (no decode attempted) | Unit test |
| NFR-DE-06 | Nonce uniqueness: 10^6 seals in a test produce 10^6 distinct nonces (CSPRNG) | Unit test asserting no duplicates in-memory |
| NFR-DE-07 | `durable_journal` append throughput ≥ 1000 entries/s at SQLite WAL-Normal on commodity hardware | Criterion bench |

### 10 Criterion Benchmarks

1. `bench_step_run_idempotent` — single `Idempotent` step, fresh + replay.
2. `bench_step_run_atleastonce` — single `AtLeastOnce` step, fresh + replay.
3. `bench_step_run_exactly_once_n` — N=5 `ExactlyOnceGuarded` steps end-to-end (CI gate: ≤ 5 ms).
4. `bench_parallel_n` — `parallel()` with N=8 concurrent steps, fresh + replay.
5. `bench_replay_cursor_n` — resume from a journal with N=5000 steps (range cursor).
6. `bench_journal_append_buffered` — `AppendBuffered` round-trip via mpsc (no DB).
7. `bench_journal_append_acked` — `AppendAcked` round-trip including WAL commit.
8. `bench_payload_seal` — `PayloadCipher::seal` for 4 KiB payload.
9. `bench_payload_open` — `PayloadCipher::open` for 4 KiB ciphertext.
10. `bench_prune_batch` — prune 10000 terminal executions in 500-row batches.

---

## Deferred Items (Post-v1 TODO Markers)

```rust
// TODO(post-v1): CompensatingStep for transactional multi-effect rollback.
// Deferred: multi-effect executions in v1 are NOT atomic; partial application is possible.
// No automatic compensation. See spec-064 §Non-Goals.

// TODO(post-v1): aggressive mid-execution compaction for very long agent sessions.
// v1 uses checkpoint-fold of committed-idempotent prefix only.

// TODO(post-v1): non-determinism lint / #[durable] macro for enforcing
// "non-deterministic reads must be wrapped in ctx.step()".
// v1 safeguard: ReplayDivergence fingerprint check (INV-3). Lint is defense-in-depth.

// TODO(post-v1): auto crash-recovery on process start (no-arg resume of in-flight executions).
// v1 fixes only the explicit /plan resume user command path (P2).

// TODO(post-v1): backend-agnostic (Postgres/:memory:/non-Unix) crash-orphan reclamation via a
// heartbeat-timeout signal ("option c"). #6254's sweep requires the INV-15 advisory flock as its
// liveness signal and is a documented no-op on lock_dir=None backends. Tracked as a separate
// follow-up issue filed after #6254 merges.
```

---

## Housekeeping Note (Out of Scope)

Pre-existing spec number collisions were resolved in issue #6308: `specs/057-agent-persistence`
was renumbered to `specs/078-agent-persistence`, and `specs/058-plugins` was renumbered to
`specs/079-plugins`. Number 064 is confirmed unused and is locked for this spec.
