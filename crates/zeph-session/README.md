# zeph-session

[![Crates.io](https://img.shields.io/crates/v/zeph-session)](https://crates.io/crates/zeph-session)
[![docs.rs](https://img.shields.io/docsrs/zeph-session)](https://docs.rs/zeph-session)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.96-blue)](https://www.rust-lang.org)

Conversation-session persistence for [Zeph](https://github.com/bug-ops/zeph): an append-only JSONL
event log, deterministic replay, and fork engine, shared by every channel (CLI, TUI, Telegram, ACP,
`zeph serve`).

> [!IMPORTANT]
> This crate is **under active construction** (spec-068, issue
> [#5343](https://github.com/bug-ops/zeph/issues/5343)). The foundation — `SessionEvent` schema,
> the JSONL event log with torn-append recovery, the `acp_sessions` metadata store, the replay
> engine, and the `Condenser` trait contract — has landed. The fork engine, the default
> `LlmCondenser`, agent-loop wiring (`SessionSink`), `zeph serve`, and the `/conv` TUI commands
> land in follow-up phases of the plan.

## Overview

Every conversation-session's history is appended as one line per event to
`<data_dir>/sessions/<session_id>/events.jsonl` — the source of truth. The existing `acp_sessions`
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
