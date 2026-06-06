# zeph-durable

[![Crates.io](https://img.shields.io/crates/v/zeph-durable)](https://crates.io/crates/zeph-durable)
[![docs.rs](https://img.shields.io/docsrs/zeph-durable)](https://docs.rs/zeph-durable)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)

Native durable execution layer for [Zeph](https://github.com/bug-ops/zeph) — journals the *control
flow* of an execution (steps, promises, timers) so a crashed or interrupted run can resume at the
point of failure instead of restarting from scratch.

> [!IMPORTANT]
> This crate is a **foundational scaffold** (spec-064, issue #4944). It currently exposes
> *type-level* building blocks only — there is **no execution behavior yet**. The journal writer,
> execution backends, replay cursor, and the durable step primitive land in follow-up issues of
> epic [#4707](https://github.com/bug-ops/zeph/issues/4707).

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
  `ExactlyOnceGuarded`).
- **config** — pure-data `DurableConfig` and `RetentionPolicy` mirroring the `[durable]` TOML
  section, with spec defaults applied on deserialization.
- **error** — the crate-wide `DurableError`.

## Architecture & invariants

- **Layer 0, no business-logic dependencies (INV-1).** `zeph-durable` MUST NOT depend on
  `zeph-llm`, `zeph-memory`, `zeph-core`, `zeph-sanitizer`, or any business-layer crate. Its only
  dependencies are `zeph-db` and `zeph-common`.
- **Closed enums make illegal states unrepresentable.** Control entries (`EffectIntent`,
  `PromiseCreated`, `TimerArmed`) carry no payload field — a "control entry with payload" cannot be
  constructed.
- **Domain-separated idempotency keys.** `IdempotencyKey::derive` uses BLAKE3 `derive_key` with a
  fixed context string and length-delimited (injective) input, so an attacker-controlled
  fingerprint cannot collide with a different `(execution_id, step_id)` pair.

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

## MSRV

Rust **1.95** (Edition 2024, resolver 3).

## License

MIT — see [LICENSE](../../LICENSE).
