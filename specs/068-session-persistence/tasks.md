---
aliases:
  - Session Persistence Tasks
  - Tasks 068
tags:
  - tasks
  - session
  - persistence
created: 2026-06-13
status: draft
related:
  - "[[068-session-persistence/plan]]"
  - "[[068-session-persistence/spec]]"
---

# Implementation Tasks 068 — Session Persistence, Event Log Replay, and `zeph serve`

Tasks are ordered by phase and dependency. Each task has: ID, phase, crate owner, description, and spec references.

---

## Phase P0 — Foundation

### T-001 — Scaffold `zeph-session` crate
**Owner:** rust-developer  
**Crate:** `zeph-session` (new)  
**Spec refs:** §3, §15, §16  
Create `crates/zeph-session/Cargo.toml` with `serde`, `serde_json`, `tokio`, `uuid`, `thiserror`. Add to workspace. Verify no dependency path to `zeph-llm`, `zeph-memory`, `zeph-core`, or **`zeph-durable`** (see plan.md "Crate Boundary Decision" — the two crates are structurally parallel but do not share code). `cargo tree -p zeph-session` must not show `zeph-durable` in the output.

### T-002 — Define `SessionEvent` + `SessionEventEnvelope`
**Owner:** rust-developer  
**Crate:** `zeph-session`  
**Spec refs:** §4.2, §4.3  
Implement `SessionEventEnvelope` and `SessionEvent` enum with all variants. Use `#[serde(tag = "kind")]`. Import `MessagePart` from `zeph-llm` and `AnchoredSummary` from `zeph-common` — do not redefine. Add schema version field for future compatibility (NFR-C2). Unit test: round-trip every variant through `serde_json`.

### T-003 — Implement `SessionEventLog` (JSONL writer)
**Owner:** rust-developer  
**Crate:** `zeph-session`  
**Spec refs:** §4.1, INV-SP-1, INV-SP-2  
Append-only JSONL file writer. One `write` + `fsync` per event. Set `0o600` file permissions on create. Implement `open_and_validate`: stream-parse all lines; truncate partial/garbled trailing line (INV-SP-2). Return validated `max_seq`. Tests: `test_append_and_read_roundtrip`, `test_torn_write_truncation`.

### T-004 — Write SQLite migration 105
**Owner:** rust-developer  
**Crate:** `zeph-db`  
**Spec refs:** §5.1  
Create `crates/zeph-db/migrations/sqlite/105_session_persistence.sql` — ALTER TABLE adds: `last_seq`, `event_count`, `forked_from`, `forked_at_seq`, `status`, `last_condensed_seq`. CREATE UNIQUE INDEX on `conversation_id WHERE NOT NULL`. Four additional indexes. Do NOT add `title` or `conversation_id` (already exist from migrations 016 and 026). Tests: `test_migration_105_sqlite`, `test_migration_105_idempotent`.

### T-005 — Write PostgreSQL migration 105
**Owner:** rust-developer  
**Crate:** `zeph-db`  
**Spec refs:** §5.1, Note N2  
Create `crates/zeph-db/migrations/postgres/105_session_persistence.sql`. NOT byte-identical to SQLite version (BIGINT, timestamp types, `IF NOT EXISTS` syntax). Same logical schema. Test on PostgreSQL fixture (NFR-C4).

### T-006 — Implement `SessionStore`
**Owner:** rust-developer  
**Crate:** `zeph-session`  
**Spec refs:** §5.3  
Implement all `SessionStore` operations: `create`, `update_seq`, `set_status`, `set_condensed_seq`, `get`, `list`, `record_fork`, `delete`. Both SQLite and PostgreSQL paths via `DatabaseDriver` trait. Unit tests for each operation against in-memory SQLite fixture.

### T-007 — Implement `ReplayEngine`
**Owner:** rust-developer  
**Crate:** `zeph-session`  
**Spec refs:** §6.1, §6.2  
Stream-parse JSONL with bounded buffer (≤ 100 events). Fold all `SessionEvent` variants into `Vec<Message>`. Respect `up_to` parameter. Apply INV-SP-2 (via `SessionEventLog::open_and_validate`) and INV-SP-3 (trigger `SessionStore` update if `last_seq` lags). Tests: `test_replay_empty_session`, `test_replay_basic_turn`, `test_replay_tool_roundtrip`, `test_replay_condensation_folds`, `test_replay_stop_at_seq`.

### T-008 — Define `Condenser` trait
**Owner:** rust-developer  
**Crate:** `zeph-session`  
**Spec refs:** §8.2  
Define `Condenser` trait with `should_condense` and `condense` async methods. Define `CondensationResult`. No `LlmCondenser` implementation in P0. Stub `NullCondenser` for testing.

---

## Phase P1 — Replay + Emit

### T-101 — Add `SessionConfig` to `zeph-config`
**Owner:** rust-developer  
**Crate:** `zeph-config`  
**Spec refs:** §16  
Add `SessionConfig` and `CondenseConfig` structs. Wire into root `Config`. Defaults: `enabled = true`, `data_dir = ".zeph/sessions"`, `encrypt = false`, `max_event_log_mb = 256`, `condense.threshold = 0.85`, `condense.keep_recent = 20`.

### T-102 — Implement `SessionSink` in `zeph-agent-persistence`
**Owner:** rust-developer  
**Crate:** `zeph-agent-persistence`  
**Spec refs:** §12.1, INV-SP-1  
`SessionSink` intercepts each turn result: (1) append `SessionEvent`s to `SessionEventLog` (log-first), (2) call `SessionStore::update_seq`, (3) call existing projection write path. Tests: `test_session_sink_log_first` (mock crash between step 1 and 3), `test_inv_sp3_projection_reconcile`.

### T-103 — Wire `SessionId` and `SessionSink` into agent startup for non-ACP channels
**Owner:** rust-developer  
**Crate:** `zeph-core`  
**Spec refs:** §12.2  
If `[session] enabled = true` and channel is non-ACP: mint `SessionId` (UUID v4), insert `acp_sessions` row, instantiate `SessionSink`. Pass `SessionSink` to the persistence subsystem. Reuse existing `SessionId` from `zeph-common` (spec 044).

### T-104 — Upgrade `do_resume_session` to use `ReplayEngine`
**Owner:** rust-developer  
**Crate:** `zeph-acp`  
**Spec refs:** §12.3  
Replace current ACP resume implementation with `ReplayEngine::replay(id, None)`. Hydrate `MessageState` from `ReconstructedState`. Existing ACP integration tests must pass unchanged (NFR-R7). Behavior change: replay is now deterministic.

### T-105 — Upgrade `sessions list` CLI with enriched output
**Owner:** rust-developer  
**Crate:** `src/` (binary)  
**Spec refs:** §10, NFR-U2  
Update `sessions list` handler to display: session ID, title, status, event count, last updated, forked_from. Golden-file test for output format (NFR-C3).

### T-106 — Add `sessions show` CLI command
**Owner:** rust-developer  
**Crate:** `src/`  
**Spec refs:** §10  
New `SessionsCommand::Show { id, from, to, events }` variant. Displays metadata and optionally event range as JSONL.

### T-107 — Upgrade `sessions resume` CLI to live replay; add `--print` flag
**Owner:** rust-developer  
**Crate:** `src/`  
**Spec refs:** §10, NFR-C3  
`sessions resume <id>`: hydrate via `ReplayEngine` and continue agent loop. `--print`: dump events to stdout (old behavior). Document breaking change in `CHANGELOG.md`. Test: `test_cli_sessions_resume_print`.

---

## Phase P2 — Fork + Condensation

### T-201 — Implement `ForkEngine`
**Owner:** rust-developer  
**Crate:** `zeph-session`  
**Spec refs:** §7.1, §7.2  
Implement `ForkEngine::fork(src_id, at_seq)`: validate cut point, eager-copy JSONL lines, copy blobs, insert child `acp_sessions` row, append `ForkPoint` to parent log, backfill projection. Tests: `test_fork_copies_events` (AC-3), `test_fork_provenance_metadata`.

### T-202 — Expose summarizer from `zeph-context`
**Owner:** rust-developer  
**Crate:** `zeph-context`  
**Spec refs:** §8.1  
Ensure `summarize_structured` and `SummarizationDeps` are `pub` in `zeph-context`. Add doc comment and example if missing. Do not duplicate the summarizer in `zeph-session`.

### T-203 — Implement `LlmCondenser`
**Owner:** rust-developer  
**Crate:** `zeph-session`  
**Spec refs:** §8.2, INV-SP-4  
`LlmCondenser` wraps `zeph-context::summarization::summarize_structured`. Reads `last_condensed_seq` from `SessionStore`. Computes range `(last_condensed_seq, current_tail]`. Emits `Condensation` event. Updates `last_condensed_seq` atomically. Tests: `test_condense_non_overlap` (AC-6), `test_condense_replay_determinism`.

### T-204 — Emit `Compaction` event from live compaction
**Owner:** rust-developer  
**Crate:** `zeph-agent-persistence`  
**Spec refs:** §8.1, INV-SP-4  
Hook into the live compaction path in `zeph-context`. When `CompactionState` completes a compaction, call `SessionSink` to emit `Compaction` event and update `last_condensed_seq`. Non-overlap enforced via the same high-water mark as `LlmCondenser`.

### T-205 — Delegate `do_fork_session` to `ForkEngine`
**Owner:** rust-developer  
**Crate:** `zeph-acp`  
**Spec refs:** §12.3  
`do_fork_session` becomes a thin caller of `ForkEngine::fork`. Behavior-preservation test: AC-9.

### T-206 — Add `sessions fork` CLI command
**Owner:** rust-developer  
**Crate:** `src/`  
**Spec refs:** §10  
New `SessionsCommand::Fork { id, at }` variant. Calls `ForkEngine::fork`, prints new session ID.

### T-207 — Add `sessions export` and `sessions import` CLI commands
**Owner:** rust-developer  
**Crate:** `src/`  
**Spec refs:** §10  
`sessions export <id> <path>`: copy `events.jsonl` to path. `sessions import <path>`: read JSONL, create new session, import events. Tests: `test_export_import_roundtrip` (AC-11), NFR-Pt3.

---

## Phase P3 — Serve Mode

### T-301 — Implement `SessionActor`
**Owner:** rust-developer  
**Crate:** `zeph-core`  
**Spec refs:** §9.2  
`SessionActor` struct: owns `Agent<LoopbackChannel>`, `Arc<SessionEventLog>`, mpsc receiver, broadcast sender. Process loop: recv `SessionCommand::Prompt` → run one agent turn (`&mut self`) → emit `SessionOutput` events to broadcast → append events to log. Spawn via `TaskSupervisor::spawn("serve.session.<id>", ...)`. Tests: `test_session_actor_fifo_ordering`.

### T-302 — Implement `LiveSessionRegistry`
**Owner:** rust-developer  
**Crate:** `zeph-core`  
**Spec refs:** §9.3  
`parking_lot::Mutex<HashMap<SessionId, SessionActorHandle>>`. Connect: look up or spawn actor. Idle eviction: `serve.evict` task under `TaskSupervisor`. Tests: `test_serve_idle_eviction` (AC-12).

### T-303 — Add `Serve(ServeArgs)` to CLI `Command` enum
**Owner:** rust-developer  
**Crate:** `src/`  
**Spec refs:** §9.1, §10  
Add `Serve(ServeArgs)` variant to `Command` enum at `cli.rs:306`. `ServeArgs`: `http_addr`, `acp`, `auth_token_from_vault`, `max_sessions`. Gate behind `session` feature flag.

### T-304 — Implement `serve.http` axum handler module
**Owner:** rust-developer  
**Crate:** `src/serve/`  
**Spec refs:** §9.4  
All 7 HTTP endpoints. Bearer auth (BLAKE3 + `subtle::ConstantTimeEq`). Per-IP rate limit. SSE for `/sessions/:id/prompt` and `/sessions/:id/events`. Tests: `test_serve_two_connections_same_session` (AC-7), `test_serve_http_auth` (NFR-S4, NFR-S5).

### T-305 — Wire `zeph serve` startup under `TaskSupervisor`
**Owner:** rust-developer  
**Crate:** `src/`  
**Spec refs:** §9.1  
Startup: init `LiveSessionRegistry`, spawn `serve.http`, optional `serve.acp`, `serve.evict`. Graceful shutdown: `supervisor.shutdown_all(30s)` on SIGTERM. Test: `test_serve_graceful_shutdown` (NFR-R8).

### T-306 — Add `/conv` commands to TUI
**Owner:** rust-developer  
**Crate:** `zeph-tui`  
**Spec refs:** §11  
Register `/conv list`, `/conv resume <id>`, `/conv fork <id>`, `/conv show <id>` in slash-command registry. Wire to `ReplayEngine` / `ForkEngine` / `SessionStore`. Spinner: `Replaying conversation…`, `Condensing history…`, `Saving session…`. Test: AC-10 (spinner visible).

### T-307 — Add `ServeConfig` to `zeph-config`
**Owner:** rust-developer  
**Crate:** `zeph-config`  
**Spec refs:** §16  
Add `ServeConfig` struct with all `[serve]` fields. Wire into root `Config`. `auth_token` field uses vault resolution pattern (never stored inline). `--init` wizard: `step_serve()`.

---

## Phase P4 — Migration + Docs

### T-401 — Legacy session bootstrap on first resume
**Owner:** rust-developer  
**Crate:** `zeph-agent-persistence`  
**Spec refs:** §17  
On resume of a session with no `events.jsonl`: write `SessionStarted` + `Condensation`-style "imported history" synthetic event. New turns append normally. Test: `test_legacy_session_gets_bootstrapped` (NFR-C1).

### T-402 — `--migrate-config` step for `[session]` and `[serve]`
**Owner:** rust-developer  
**Crate:** `zeph-config`  
**Spec refs:** §17  
New migration step: add `[session]` and `[serve]` blocks with defaults if absent. Tests: `test_migrate_config_adds_session_block`, `test_migrate_config_idempotent`.

### T-403 — `--init` wizard steps for `[session]` and `[serve]`
**Owner:** rust-developer  
**Crate:** `zeph-config`  
**Spec refs:** §17  
Interactive prompts: `step_session()` (data_dir, enabled, condense.threshold), `step_serve()` (http_addr, require_auth, max_sessions).

### T-404 — mdBook user documentation chapters
**Owner:** rust-agents:tech-writer  
**Crate:** `docs/`  
**Spec refs:** all  
New chapters: "Session Persistence and Resume", "`zeph serve` — Persistent Agent Service". Update "CLI Reference". Run `mdbook build` to verify.

### T-405 — Update README.md and crate README
**Owner:** rust-agents:tech-writer  
**Crate:** root + `crates/zeph-session/`  
**Spec refs:** §1  
Add feature description to root `README.md`. Create `crates/zeph-session/README.md`. Use `/readme-generator` skill.

### T-406 — Final playbook run + coverage-status sweep
**Owner:** rust-agents:rust-live-tester  
**Refs:** `plan.md`, `.local/testing/playbooks/session-persistence.md`  
Execute all AC-1 through AC-13 scenarios against the live binary. Update coverage-status rows in place.

---

## Optional Tasks (non-blocking, P2)

### T-OPT-001 — Extract JSONL line-framing helper to `zeph-common`
**Owner:** rust-developer  
**Crate:** `zeph-common`  
**Spec refs:** §16 (zeph-common row), plan.md "Crate Boundary Decision"  
**Status: OPTIONAL — does not block any phase gate or spec acceptance.**

Extract the INV-SP-2 logic (JSONL line-framing + torn-tail truncation: read lines, validate complete-JSON-terminated-by-`\n`, truncate a partial final line) into a small, storage-agnostic helper in `zeph-common` (~40–60 LOC). This helper has no session-specific fields and can be reused by any future JSONL consumer. It must NOT depend on `zeph-durable` or any session types.

Constraints:
- If the helper starts growing session-specific fields, move it back into `zeph-session`.
- Do NOT genericize further into a `JsonlLog<E>` abstraction (YAGNI pre-1.0 per CLAUDE.md rules).
- If a third JSONL-append consumer appears post-1.0, revisit the full extraction at that point.

Can be implemented as a sub-task of T-003 or as a standalone follow-up PR after P0.

---

## Task Dependency Summary

```
T-001 → T-002 → T-003 → T-007
T-001 → T-006
T-004, T-005 → T-006

T-101, T-006, T-007 → T-102 → T-103
T-007 → T-104
T-006 → T-105, T-106
T-006, T-007 → T-107

T-007 → T-201
T-202 → T-203
T-203, T-204 → (INV-SP-4 enforcement complete)
T-201 → T-205, T-206, T-207

T-103, T-201 → T-301 → T-302 → T-303 → T-304 → T-305
T-302 → T-306
T-307 independent of actor impl

T-102 → T-401
T-101 → T-402 → T-403
T-404, T-405 after P3 PR merged
T-406 after P4 PR merged
```
