# zeph-durable

[![Crates.io](https://img.shields.io/crates/v/zeph-durable)](https://crates.io/crates/zeph-durable)
[![docs.rs](https://img.shields.io/docsrs/zeph-durable)](https://docs.rs/zeph-durable)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

Native durable execution layer for [Zeph](https://github.com/bug-ops/zeph) — journals the *control
flow* of an execution (steps, promises, timers) so a crashed or interrupted run can resume at the
point of failure instead of restarting from scratch.

> [!NOTE]
> Spec-064 (epic [#4707](https://github.com/bug-ops/zeph/issues/4707)) is complete — all 11 child
> issues shipped and the epic is closed. The type-level foundation, the AEAD payload contract, the
> persistence engine (`LocalBackend`, the background `JournalWriter` actor, the sealed
> `ExecutionBackend` dispatcher), the execution heart (the `&self` `DurableContext` with
> deterministic step ids, the fingerprint-guarded replay cursor, the exactly-once intent/result
> protocol, and `parallel()` batches), the promise/timer layer (`DurablePromise`, `DurableHandle`,
> `DurableTimerService`), and journal retention (`DurableRetentionService`, including the
> flock-verified crash-orphan staleness sweep) have all landed. The `zeph durable` CLI (`list` /
> `show` / `inspect` / `prune` / `resume` / `cancel`) and the TUI durable-execution widget are
> wired, and all four consuming adapters — agent tool loop, orchestration (`/plan resume`),
> scheduler, and subagent — journal their steps through `DurableContext`. `zeph durable cancel <id>`
> (issue [#6362](https://github.com/bug-ops/zeph/issues/6362)) durably marks a specific execution as
> intentionally stopped via a terminal `Canceled` status, distinct from the crash-driven `Aborted`
> state, so crash-resume sweeps never resurrect it.

## Overview

`zeph-durable` is a Layer-0 infrastructure crate, analogous to `zeph-db` and `zeph-common`. It is a
pure infrastructure primitive: it sees opaque serialized payloads, never domain types. Domain
meaning lives in thin adapter modules inside each consuming crate (the agent tool-loop,
orchestration, scheduler, and subagent layers).

The eventual design provides a `DurableContext` facade (`step()` / `parallel()` / `promise()` /
`sleep_until()`), an explicit `EffectClass` contract per step, a background journal-writer actor
with group-commit, AEAD payload encryption, and a fingerprint-guarded replay cursor — all backed by
a dedicated `durable.db` (SQLite) or a feature-gated Restate backend.

## Key Modules

- **ids** — journal-boundary newtypes: `ExecutionId` / `PromiseId` / `TimerId` (UUIDv7), `StepId`,
  `JournalSeq`, `IdempotencyKey`, and the `ExecutionKind` discriminator. Private fields, smart
  constructors, serde-round-trip stable.
- **journal** — the `Journal` trait plus its data model: `JournalEntry`, the closed `EntryKind`
  enum, and `ExecutionStatus`.
- **effect** — `EffectClass`, the per-step side-effect contract (`Idempotent` / `AtLeastOnce` /
  `ExactlyOnceGuarded`), plus `EffectIntentSubClass` and the `OnAmbiguous` policy that govern the
  ambiguous window.
- **step** — the durable step typestate: `StepDescriptor` (with the construction-time ambiguity
  rule), `StepHandle` (exposes the idempotency key for boundary dedup), `StepError`, the
  `Live`/`Replayed` `StepOutcome`, and the `DurableStep` record.
- **handle** — the `&self` `DurableContext` front door: `step()` / `step_recorded()` /
  `parallel()`, deterministic `AtomicU32` step ids, a BLAKE3 replay-divergence guard, the
  exactly-once intent/result protocol, and the `ParallelScope` for completion-order-independent
  batches.
- **cipher** — the `PayloadCipher` AEAD seal/open contract, the `PayloadAad` location binding, and
  the read-side `ensure_payload_within_limit` guard. The concrete cipher lives in a consuming crate
  (INV-1).
- **config** — re-exports the pure-data `DurableConfig`, `RetentionPolicy`, and `DurableBackend`
  types (defined in `zeph-config`, mirroring the `[durable]` TOML section with spec defaults
  applied on deserialization) and adds `encryption_gate` / `EncryptionGate`, which resolves whether
  payload encryption is optional, required, or unavailable for a given backend + config pair.
- **backend** — the sealed `ExecutionBackend` trait, `BackendCapabilities`, the `DurableBackendEnum`
  enum dispatcher, and `LocalBackend` (a dedicated `durable.db` pool implementing `Journal`, sealing
  payloads through the injected cipher). Includes `open_execution_exclusive`, a `flock(2)`-backed
  process-exclusivity lock that rejects a second concurrent holder for the same `ExecutionId`
  (guards against colliding `agent_turn` executions across processes).
- **writer** — the background `JournalWriter` actor and its cloneable `JournalWriterHandle`:
  group-commit for buffered appends, flush-before-commit ACKs for exactly-once entries, and
  `MAX(seq)` restart resume.
- **promise** — the durable promise primitive: `DurablePromise<T>` (a journaled, resumable await
  point) and `DurableHandle` for out-of-band resolution.
- **timer** — `DurableTimerService`, a polling actor that fires journaled timers on resume.
- **retention** — `DurableRetentionService`, the background pruner that enforces `RetentionPolicy`
  (TTL, execution/journal-byte caps) against the `durable.db` pool. Also folds in the crash-orphan
  staleness sweep: a `stale_running_after_secs` knob reclaims `status='running'` rows abandoned by
  an ungraceful process exit, gated on a non-blocking advisory-lock liveness probe so a still-live
  owner is never aborted out from under it.
- **error** — the crate-wide `DurableError`.

## Architecture & invariants

- **Layer 0, no business-logic dependencies (INV-1).** `zeph-durable` MUST NOT depend on
  `zeph-llm`, `zeph-memory`, `zeph-core`, `zeph-sanitizer`, or any business-layer crate. Its only
  direct `zeph-*` dependency is `zeph-db`; the rest are infrastructure crates (`tokio`, `tracing`,
  `metrics`, `bytes`, `blake3`, `serde`, `uuid`). The concrete payload cipher lives in `zeph-core`.
- **Closed enums make illegal states unrepresentable.** Control entries (`EffectIntent`,
  `PromiseCreated`, `TimerArmed`) carry no payload field — a "control entry with payload" cannot be
  constructed.
- **Domain-separated idempotency keys.** `IdempotencyKey::derive` uses BLAKE3 `derive_key` with a
  fixed context string and length-delimited (injective) input, so an attacker-controlled
  fingerprint cannot collide with a different `(execution_id, step_id)` pair.
- **Tamper-evident replay (issue #6360).** `LocalBackend::with_hwm_key` attaches an authenticated
  per-execution high-water-mark (HWM) — a signed `{execution_id, max_committed_step_id,
  committed_result_count, key_epoch}` tuple, verified O(1) on every resume — that detects
  deletion of a committed `StepResult` row, including across a `checkpoint_fold` compaction. It
  activates unconditionally whenever `ZEPH_DURABLE_KEY` is provisioned, unlike the row-HMAC above
  which stays opt-in for shared-database deployments.
- **Downgrade-resistant, vault-sealed (issue #6449).** `LocalBackend::with_integrity_sealed`/
  `with_grandfather` close the gap the HWM alone left open: deleting the whole
  `durable_execution_integrity` row used to be trusted as "predates the feature." Once an
  operator runs `zeph durable seal-integrity` (which refuses while any resumable execution has
  committed results but no integrity row), an absent row on a keyed, non-grandfathered execution
  with ≥1 committed `StepResult` is unconditional tamper — the seal marker and grandfather set
  are vault-stored, never a DB column, so a DB-write attacker cannot forge or evade them.
  Residual: a grandfathered `execution_id` remains a *permanent* forge-able slot (an explicit,
  documented operator opt-out, not free protection) — prefer draining where practical.
- **Windowed key rotation (issue #6460).** `LocalBackend::with_previous_hmac_key` and
  `with_previous_hwm_key` register a previous key alongside the current one, mirroring the AEAD
  cipher's own `previous` slot: verification tries the current key then the previous key, so
  `zeph durable rotate-key` no longer force-aborts every in-flight execution the moment it runs.
  Three drop-scans (control-entry HMAC, high-water-mark, and post-rotation `checkpoint_fold`
  compaction) back `rotate-key --drop-previous`'s default-on safety check before the window closes.

> [!NOTE]
> **Schema ownership (INV-14).** `zeph-durable` owns **no** `.sql` files and **no**
> `sqlx::migrate!`. The four `durable_*` tables (`durable_executions`, `durable_journal`,
> `durable_promises`, `durable_timers`) live as numbered migrations in
> `zeph-db/migrations/{sqlite,postgres}/` and are applied via `zeph_db::run_migrations` against a
> dedicated `durable.db` pool.

## Installation

This crate is an internal workspace member of Zeph. To use it from another workspace crate:

```toml
[dependencies]
zeph-durable = { path = "../zeph-durable" }
# or with the postgres backend:
zeph-durable = { path = "../zeph-durable", default-features = false, features = ["postgres"] }
```

## Feature Flags

Backend selection is forwarded to `zeph-db`; exactly one backend is active at a time.

| Feature | Description | Default |
|---------|-------------|---------|
| `sqlite` | Enables the SQLite backend via `zeph-db/sqlite` | Yes |
| `postgres` | Enables the PostgreSQL backend via `zeph-db/postgres` | No |

> [!WARNING]
> `sqlite` and `postgres` are mutually exclusive (enforced by `zeph-db`). Building with
> `--all-features` is intentionally unsupported — use `--features full` or `--features full,postgres`.

## Usage

Idempotency keys are deterministic for a given `(execution, step, fingerprint)` and domain-separated
from any other BLAKE3 use:

```rust
use zeph_durable::{ExecutionId, IdempotencyKey, StepId};

let execution = ExecutionId::new(); // fresh, time-ordered UUIDv7

let key = IdempotencyKey::derive(execution, StepId::new(0), b"tool:read_file");
assert_eq!(
    key,
    IdempotencyKey::derive(execution, StepId::new(0), b"tool:read_file"),
);
```

Configuration deserializes from the `[durable]` TOML table with every field defaulted to its spec
value:

```rust
use zeph_durable::DurableConfig;

let cfg: DurableConfig = toml::from_str("").unwrap(); // empty table => all defaults
assert!(!cfg.enabled);
assert_eq!(cfg.journal_ack_timeout_ms, 5_000);
assert_eq!(cfg.max_payload_bytes, 1_048_576);
```

A `DurableContext` wraps each unit of work in a step. A fresh run executes the closure and journals
its result; a resumed run replays the journaled result without re-running it. The closure receives a
`StepHandle` carrying the step's idempotency key for boundary deduplication:

```rust,ignore
use zeph_durable::{DurableContext, EffectIntentSubClass, OnAmbiguous, StepDescriptor};

// Read-only work is idempotent and replays for free.
let preview: String = ctx
    .step(StepDescriptor::idempotent("read_head", b"tool:read:/var/log".to_vec()),
          |_handle| async { Ok(read_first_line().await?) })
    .await?;

// A paid call is exactly-once-guarded: its intent is journaled before the call and its result
// after, and the idempotency key is forwarded to the provider for boundary dedup.
let reply: String = ctx
    .step(
        StepDescriptor::exactly_once_guarded(
            "llm_call",
            EffectIntentSubClass::CostBearingOrBoundaryIdempotent,
            Some(OnAmbiguous::Skip),
            b"llm:gpt:summarize".to_vec(),
        )?,
        |handle| async move { Ok(call_provider(handle.idempotency_key()).await?) },
    )
    .await?;
```

## MSRV

Rust **1.98** (Edition 2024, resolver 3).

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
