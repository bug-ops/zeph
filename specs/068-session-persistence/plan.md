---
aliases:
  - Session Persistence Plan
  - Plan 068
tags:
  - plan
  - session
  - persistence
created: 2026-06-13
status: draft
related:
  - "[[068-session-persistence/spec]]"
  - "[[068-session-persistence/tasks]]"
---

# Implementation Plan 068 — Session Persistence, Event Log Replay, and `zeph serve`

## Overview

Five phases, each producing a self-contained PR with full CI gate (fmt/clippy/nextest/rustdoc per `branching.md`). No phase begins until the previous PR is merged.

Each phase includes mandatory: `spec.md` compliance check, playbook update, coverage-status update, CHANGELOG entry.

---

## Crate Boundary Decision

**Decision: Option A — new `zeph-session` crate. `zeph-durable` is untouched.**

This decision was evaluated against the actual `zeph-durable` source (1661 LOC across `journal.rs`, `replay.rs`, `writer.rs`, `backend.rs`) and confirmed by the architect (handoff `2026-06-13T23-45-46-architect.md`). The reasoning is recorded here so reviewers do not re-litigate it.

The two crates are structurally similar in shape (append-only log, replay cursor, supervised writer actor) but differ on three independent axes that make code reuse a poor fit:

1. **Storage engine:** `zeph-durable` is SQLite-backed (`durable.db`, dedicated pool, `INSERT`/`read_execution_range`). `zeph-session` uses append-only JSONL files — required by #2807 for greppability and exportability. None of `backend.rs` transfers.
2. **Replay semantics:** `zeph-durable`'s `ReplayCursor` is a `StepId`-keyed idempotency arbiter (`OnAmbiguous` Skip/Fail, `EffectClass`, intent/result pairing). `zeph-session`'s `ReplayEngine` is a linear forward fold into `Vec<Message>` with no re-execution — no `StepId`, no ambiguity, no effect class. Approximately 90% of `replay.rs` is step/effect logic that session replay does not have.
3. **Payload format:** `zeph-durable` stores AEAD-sealed opaque `Bytes` with per-row HMAC. `zeph-session` stores domain-typed readable JSON (`UserMessage`, `AssistantMessage`, `ToolCall`, etc.) — encryption is opt-in and deferred.

The real overlap is ~80–120 LOC of generic "append a line / read lines / supervised actor" *idiom* — a pattern, not copy-pasted machinery. This is acceptable (comparable to both crates independently using `Result` or `tokio::mpsc`). The spec states this explicitly (§16 Affected Subsystems) so future reviewers understand the decision.

**INV-1 (spec-064 §NEVER)** is a hard constraint: `zeph-durable` MUST NOT depend on agent types (`Message`, `MessagePart`, `SessionId`, etc.). Options C and D would require widening or violating INV-1; both are rejected on architectural grounds.

**Optional non-blocking extraction (P2, not required):** A tiny `zeph-common` helper for JSONL line-framing + torn-tail truncation (the INV-SP-2 logic, ~40–60 LOC, storage-agnostic) could be extracted as a sub-task of the `zeph-session` work. This is a nice-to-have — `zeph-session` can implement INV-SP-2 inline if the helper is not extracted. It does NOT block spec acceptance or any phase gate. If a third append-only JSONL consumer appears post-1.0, revisit the full `JsonlLog<E>` generic (YAGNI before that point).

---

## P0 — Foundation (PR 1)

**Goal:** New `zeph-session` crate with event-log I/O and metadata store. No agent wiring. Fully unit-tested in isolation.

**Branch:** `feat/m*/2807-P0-session-foundation`

**Reference implementation (read before writing):** Review `crates/zeph-durable/src/writer.rs` (`JournalWriter` actor pattern), `crates/zeph-durable/src/replay.rs` (`ReplayCursor` 100-step segment buffer), and `crates/zeph-durable/src/journal.rs` (append + fsync pattern). `zeph-session` mirrors these primitives at the conversation-semantics level. Do NOT add `zeph-durable` as a Cargo dependency — copy the pattern, not the code. See spec-068 §3 and §14 for the architectural rationale.

### Deliverables

1. **`crates/zeph-session/`** — new crate scaffolded:
   - `Cargo.toml` with no dependency on `zeph-llm`, `zeph-memory`, or `zeph-core`
   - `src/lib.rs`, `src/event.rs` — `SessionEvent`, `SessionEventEnvelope` (serde `#[serde(tag = "kind")]`)
   - `src/log.rs` — `SessionEventLog`: append-only JSONL writer (fsync per event, file perms `0o600`)
   - `src/store.rs` — `SessionStore`: `acp_sessions` CRUD via `zeph-db` pool
   - `src/replay.rs` — `ReplayEngine::replay` (stream-parse JSONL, INV-SP-2 truncation, fold)
   - `src/condenser.rs` — `Condenser` trait + `CondensationResult` struct (no `LlmCondenser` impl yet)
   - `src/error.rs` — `SessionError` enum

2. **`crates/zeph-db/migrations/sqlite/105_session_persistence.sql`**
3. **`crates/zeph-db/migrations/postgres/105_session_persistence.sql`**

4. **Tests** (all in `zeph-session`):
   - `test_append_and_read_roundtrip` — write N events, reopen, read all N
   - `test_torn_write_truncation` — truncate last line at various byte offsets, verify clean open
   - `test_replay_empty_session` — replay empty log returns `ReconstructedState` with 0 messages
   - `test_replay_basic_turn` — `SessionStarted` + `UserMessage` + `AssistantMessage` → 2-message state
   - `test_replay_tool_roundtrip` — `ToolCall` + `ToolResult` → parts appended correctly
   - `test_replay_condensation_folds` — `Condensation` event replaces range in fold
   - `test_replay_stop_at_seq` — `up_to=N` stops at N events
   - `test_inv_sp4_no_overlap` — condense twice; assert second range starts after first
   - `test_migration_105_sqlite` — migration runs clean on in-memory SQLite
   - `test_migration_105_idempotent` — migration runs twice without error

5. **`zeph-session` added to workspace `Cargo.toml`**

### Acceptance Criteria
- `cargo nextest run -p zeph-session` → all tests PASS
- `cargo doc -p zeph-session` → 0 warnings with `RUSTDOCFLAGS=--deny rustdoc::broken_intra_doc_links`
- `cargo tree -p zeph-session` → no `zeph-llm`, `zeph-memory`, `zeph-core` in dependency tree
- Migration 105 runs on existing SQLite fixtures without error
- NFR-M1, NFR-R6 satisfied

---

## P1 — Replay + Emit (PR 2)

**Goal:** Wire the event log into the agent loop. Enable `sessions list/show/resume` commands.

**Branch:** `feat/m*/2807-P1-replay-emit`

### Deliverables

1. **`zeph-agent-persistence`** — `SessionSink` struct:
   - Implements dual-write (INV-SP-1): log append before SQLite projection write
   - Accepts `Arc<SessionEventLog>` + `SessionStore` reference
   - Emits `UserMessage`, `AssistantMessage`, `ToolCall`, `ToolResult` events per turn
   - Calls `SessionStore::update_seq` after flush

2. **`zeph-core`** — agent startup:
   - If `[session] enabled = true` and channel is non-ACP, mint `SessionId` + insert `acp_sessions` row
   - Instantiate `SessionSink`, pass to persistence path

3. **`zeph-acp`** — `do_resume_session`:
   - Delegate to `ReplayEngine::replay(id, None)`
   - Hydrate `MessageState` from `ReconstructedState`
   - Verify behavior-preservation: existing ACP integration tests pass unchanged

4. **CLI** — `src/commands/sessions.rs`:
   - `sessions list` — enrich with `title`, `status`, `event_count`, `forked_from`
   - `sessions show <id> [--from N] [--to N] [--events]` — new
   - `sessions resume <id> [--print]` — upgrade to live replay hydrate; `--print` dumps to stdout

5. **`zeph-config`** — add `SessionConfig` and `CondenseConfig` structs; wire into `Config`

6. **Tests** (addition to P0 suite):
   - `test_session_sink_log_first` — mock crash between log and projection; verify INV-SP-1
   - `test_inv_sp3_projection_reconcile` — write event to log; skip projection; open; verify reconcile
   - `test_cli_sessions_list` — golden-file test for list output format
   - `test_cli_sessions_resume_print` — `--print` dumps JSONL, no agent started (NFR-C3)

### Acceptance Criteria
- AC-1 (crash survival), AC-4 (torn write), AC-5 (projection reconcile), AC-13 (--print compat)
- NFR-R1, NFR-R2, NFR-R3, NFR-R4, NFR-C3 satisfied
- ACP integration tests (NFR-R7): `cargo nextest run -p zeph-acp` all PASS

---

## P2 — Fork + Condensation (PR 3)

**Goal:** Fork engine, LLM-backed condenser, `Compaction` event hook. CLI `sessions fork/export/import`.

**Branch:** `feat/m*/2807-P2-fork-condensation`

### Deliverables

1. **`zeph-session`** — `ForkEngine::fork`:
   - Eager copy semantics (§7.2 algorithm)
   - Blob hard-link or copy for referenced image refs
   - `SessionStore::record_fork` call
   - `ForkPoint` event appended to parent log

2. **`zeph-session`** — `LlmCondenser`:
   - Implements `Condenser` trait
   - Reuses `zeph-context::summarization::summarize_structured` (DRY — no summarizer duplication)
   - Reads `last_condensed_seq`; respects INV-SP-4
   - Config: `condense_provider`, `threshold`, `keep_recent`

3. **`zeph-context`** — expose `summarize_structured` / `SummarizationDeps` as `pub` API (if not already)

4. **`zeph-agent-persistence`** — emit `Compaction` event when live compaction fires; update `last_condensed_seq`

5. **`zeph-acp`** — `do_fork_session` delegates to `ForkEngine::fork` (behavior-preservation tests required)

6. **CLI**:
   - `sessions fork <id> [--at <seq>]`
   - `sessions export <id> <path.jsonl>`
   - `sessions import <path.jsonl>`

7. **Tests**:
   - `test_fork_copies_events` — AC-3
   - `test_fork_provenance_metadata` — child `acp_sessions.forked_from` matches parent
   - `test_condense_non_overlap` — AC-6
   - `test_condense_replay_determinism` — condense then replay; verify identical `MessageState`
   - `test_export_import_roundtrip` — AC-11
   - `test_acp_fork_delegates_correctly` — AC-9

### Acceptance Criteria
- AC-3 (fork at N), AC-6 (non-overlap), AC-9 (ACP delegation), AC-11 (export/import)
- NFR-P4 (fork latency), NFR-R5 (condensation non-overlap), NFR-R7 (ACP tests pass)

---

## P3 — Serve Mode (PR 4)

**Goal:** `zeph serve` under `TaskSupervisor`, HTTP/SSE API, `LiveSessionRegistry`, TUI `/conv` commands.

**Branch:** `feat/m*/2807-P3-serve`

### Deliverables

1. **`zeph-core`** — `SessionActor`:
   - Owns `Agent<LoopbackChannel>` (exclusive `&mut`)
   - mpsc `Receiver<SessionCommand>` in, broadcast `Sender<SessionOutput>` out
   - Spawned via `TaskSupervisor::spawn("serve.session.<id>", ...)`
   - Processes `Prompt` → run one turn → emit events → append to log

2. **`zeph-core`** — `LiveSessionRegistry`:
   - `parking_lot::Mutex<HashMap<SessionId, SessionActorHandle>>`
   - Never held across `.await`
   - Connect/attach/spawn logic
   - Idle eviction: `spawn_restartable` for `serve.evict` task

3. **`src/`** (binary) — `zeph serve` command:
   - `Serve(ServeArgs)` variant added to `Command` enum at `cli.rs:306`
   - `ServeArgs`: `http_addr`, `acp` flag, `auth_token_from_vault`, `max_sessions`
   - Startup: initialize `LiveSessionRegistry`, spawn `serve.http` + optional `serve.acp` + `serve.evict` under `TaskSupervisor`
   - Graceful shutdown: `supervisor.shutdown_all(30s)` on SIGTERM

4. **`src/serve/`** — axum HTTP handler module:
   - All 7 endpoints (§9.4)
   - Bearer auth via BLAKE3 + `subtle::ConstantTimeEq`
   - Per-IP rate limiting (reuse `zeph-gateway` pattern)
   - SSE streaming for `/sessions/:id/prompt` and `/sessions/:id/events`

5. **`zeph-tui`** — `/conv` commands:
   - `/conv list`, `/conv resume <id>`, `/conv fork <id>`, `/conv show <id>`
   - Spinner: `Replaying conversation…`, `Condensing history…`, `Saving session…`

6. **`zeph-config`** — `ServeConfig` struct; `--init` wizard prompts for `[serve]`; `--migrate-config` step

7. **Tests**:
   - `test_serve_two_connections_same_session` — AC-7
   - `test_serve_idle_eviction` — AC-12
   - `test_serve_http_auth` — NFR-S4, NFR-S5
   - `test_session_actor_fifo_ordering` — two concurrent prompts queued, delivered in order
   - `test_serve_graceful_shutdown` — NFR-R8

### Acceptance Criteria
- AC-7 (concurrent connections), AC-10 (TUI spinner), AC-12 (idle eviction)
- NFR-P6 (serve latency overhead), NFR-P7 (idle actor memory), NFR-P8 (10 concurrent sessions)
- NFR-S3, NFR-S4, NFR-S5 (auth requirements)

---

## P4 — Migration + Docs (PR 5)

**Goal:** Legacy session backfill, `--migrate-config`, `--init` wizard, documentation.

**Branch:** `feat/m*/2807-P4-migration-docs`

### Deliverables

1. **`zeph-agent-persistence`** — lazy event-log bootstrap for resumed legacy sessions:
   - On first resume of a session with no `events.jsonl`, write `SessionStarted` + a `Condensation`-style "imported history" event
   - New turns append normally after

2. **`zeph-config`** — `--migrate-config` step:
   - Add `[session]` block with defaults if absent
   - Add `[serve]` block with defaults if absent
   - Add migration step number (next after current highest)

3. **`zeph-config`** — `--init` wizard:
   - `step_session()`: prompt for `data_dir`, `enabled`, `condense.threshold`
   - `step_serve()`: prompt for `http_addr`, `require_auth`, `max_sessions`

4. **`docs/src/`** — update mdBook user docs:
   - New chapter: "Session Persistence and Resume"
   - New chapter: "`zeph serve` — Persistent Agent Service"
   - Update "CLI Reference" with new `sessions` verbs and `serve`

5. **`README.md`** and `crates/zeph-session/README.md` — feature descriptions

6. **Tests**:
   - `test_legacy_session_gets_bootstrapped` — first resume of pre-105 session writes synthetic events
   - `test_migrate_config_adds_session_block`
   - `test_migrate_config_idempotent`

### Acceptance Criteria
- AC-8 (migration idempotency)
- NFR-C1 (legacy ACP sessions resumable)
- All AC-1 through AC-13 exercised in the final playbook run
- `docs/` build passes: `mdbook build`

---

## Cross-Phase Requirements (all phases)

Before every PR:
1. `cargo +nightly fmt --check`
2. `cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,session" -- -D warnings`
3. `cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler,session" --lib --bins`
4. `RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler,session"`
5. Update `CHANGELOG.md [Unreleased]`
6. Update `.local/testing/playbooks/session-persistence.md`
7. Update `.local/testing/coverage-status.md` (rows in place, no new headers)
