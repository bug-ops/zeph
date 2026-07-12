# zeph-session

[![Crates.io](https://img.shields.io/crates/v/zeph-session)](https://crates.io/crates/zeph-session)
[![docs.rs](https://img.shields.io/docsrs/zeph-session)](https://docs.rs/zeph-session)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Conversation-session persistence for [Zeph](https://github.com/bug-ops/zeph): an append-only JSONL
event log, deterministic replay, and fork engine, shared by every channel (CLI, TUI, Telegram, ACP,
`zeph serve`).

> [!NOTE]
> Implements spec-068 (issue [#5343](https://github.com/bug-ops/zeph/issues/5343)): the
> `SessionEvent` schema, the JSONL event log with torn-append recovery, the `acp_sessions`
> metadata store, the deterministic replay engine, the `Condenser` trait contract and its default
> `LlmCondenser`, and the eager-copy `ForkEngine`. It is consumed by `zeph-core` (agent-loop
> `SessionSink` wiring, `zeph serve` per-session actors, `/conv` commands) and `zeph-acp`
> (session load/list/fork/resume handlers).

## Overview

Every conversation-session's history is appended as one line per event to
`<data_dir>/<session_id>/events.jsonl` — the source of truth. The existing `acp_sessions`
table (promoted from ACP-only to channel-agnostic, per spec-068 Decision D1) tracks lightweight
queryable metadata (`last_seq`, `status`, fork provenance) so `sessions list` and reconciliation on
open don't require replaying every log.

Replay never calls the LLM or a tool executor: it folds previously recorded events into
agent-ready `Message`s, which is the correctness guarantee behind byte-identical resume and fork.

## Architectural placement

`zeph-session` mirrors the append-only journal design of `zeph-durable` (sequential ordering,
single-writer actor model) but is a **separate** crate — the two record different concerns
(task/step effect-idempotency vs. conversation semantics) at different abstraction levels and use
different storage formats. `zeph-session` does not depend on `zeph-durable`, and vice versa.

See `specs/068-session-persistence/spec.md` and `plan.md` for the full design and phased rollout.

## Module map

| Module | Description |
|--------|-------------|
| `event` | `SessionEvent` tagged enum and its `SessionEventEnvelope` on-disk wrapper |
| `log` | `SessionEventLog` — append-only JSONL writer/reader with torn-append truncation (INV-SP-2) |
| `store` | `SessionStore` — CRUD over the `acp_sessions` metadata index |
| `replay` | `ReplayEngine` — deterministic fold of an event log into agent-ready messages; never calls the LLM |
| `condenser` | `Condenser` trait contract and the non-overlap guard (INV-SP-4) |
| `llm_condenser` | `LlmCondenser` — default `Condenser`, reusing `zeph_context::summarization` |
| `fork` | `ForkEngine` — eager-copy session forking |
| `error` | `SessionError` — crate-wide error enum |

## Usage

The on-disk layout for one session is derived from the data directory and session id:

```rust
use std::path::Path;

let dir = zeph_session::session_dir(Path::new(".zeph/sessions"), "abc-123");
assert_eq!(dir, Path::new(".zeph/sessions/abc-123"));
```

## Features

Exactly one storage backend must be selected for the `acp_sessions` metadata index; `sqlite` is the default.

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend for `zeph-db` |
| `postgres` | no | PostgreSQL backend for `zeph-db` |

## Installation

```bash
cargo add zeph-session
```

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
