---
aliases:
  - Session Persistence Spec
  - Spec 068
tags:
  - sdd
  - session
  - persistence
  - replay
  - serve
created: 2026-06-13
status: draft
related:
  - "[[064-durable-execution]]"
  - "[[013-acp]]"
  - "[[039-vault]]"
  - "[[002-agent-loop]]"
  - "[[044-zeph-common]]"
  - "[[022-zeph-context]]"
  - "[[032-database-abstraction]]"
  - "[[007-channels/spec]]"
  - "[[011-tui/spec]]"
issues:
  - "#2807"
  - "#3102"
  - "#3074"
---

# Spec 068 — Session Persistence, Event Log Replay, and `zeph serve`

## 1. Purpose

This specification defines:

1. **Session persistence** (#2807): Conversation-session identity, append-only JSONL event log, and resume/replay semantics that make every conversation crash-safe and reproducible.
2. **Immutable event log + context condensation** (#3102): A durable event log as the source of truth for conversation history, with a `Condenser` trait that persists condensation as replayable events.
3. **`zeph serve` mode** (#3074): A persistent background service that multiplexes named conversation-sessions over HTTP/SSE and ACP via a per-session actor model.

These three features share a single data model and are specified together because their invariants are mutually dependent.

---

## 2. Terminology and Existing Concept Reconciliation

Four distinct "session" concepts already exist in the codebase. The spec MUST NOT conflate them.

| Concept | Existing name | What it is | Where |
|---------|--------------|------------|-------|
| **Conversation-session** | `acp_sessions` table, `SessionId` (TEXT/uuid) | One persistent conversation thread with an event log | `crates/zeph-acp`, `zeph-db` migrations |
| **TUI tab/slot** | `SessionSlot`, `SessionRegistry` (`SlotId`) | A UI tab holding transcript + input + scroll state (in-memory only) | `crates/zeph-tui/src/session.rs` |
| **Fleet/agent session** | `agent_sessions` table, `AgentSessionRow` | Fleet-dashboard accounting row for a subagent run (lifecycle telemetry) | `src/fleet_session.rs`, `zeph-memory` |
| **Durable execution** | `JournalWriter`, `durable.db` | Effect-idempotency journal for tool re-execution (separate from conversation semantics) | `zeph-durable` |

**Decision D1 — No new `sessions` table.** The existing `acp_sessions` table (established in migration 013) is promoted from ACP-only to the **channel-agnostic conversation-session identity**. Non-ACP channels (CLI, TUI, Telegram) mint an ACP-style `SessionId` (UUID v4) on first turn and write an `acp_sessions` row. The table name remains `acp_sessions` for backward compatibility; cosmetic rename to `conversation_sessions` is deferred to a post-1.0 migration (P4).

**Decision D2 — Command namespace.** TUI already owns `/session new|next|prev|close` for tab management. New persistence commands therefore use the `/conv` prefix (short for "conversation") to avoid collision:
- TUI: `/conv list`, `/conv resume <id>`, `/conv fork <id> [--at <seq>]`, `/conv show <id>`
- CLI: extend existing `SessionsCommand` (add `show`, `fork`, `export`, `import` variants)

**Invariant INV-D1:** No code path shall introduce a fourth concurrent "session" concept without updating this terminology table and resolving naming collisions in all affected command registries.

---

## 3. Architecture Overview

```
┌───────────────────────────────────────────────────────┐
│  zeph-session  (new crate)                             │
│                                                        │
│  SessionEventLog  ──JSONL──▶  <data_dir>/<session_id>/ │
│  SessionStore     ──────────▶  acp_sessions (SQLite)   │
│  ReplayEngine     (fold events → ReconstructedState)   │
│  ForkEngine       (eager copy + new acp_sessions row)  │
│  Condenser trait + LlmCondenser                        │
└────────┬───────────────────────────────────────────────┘
         │ used by
┌────────▼───────────────────────────────────────────────┐
│  zeph-core  SessionActor  (per conversation)           │
│                                                        │
│  owns: Agent<LoopbackChannel>  (&mut, exclusive)       │
│  in:   mpsc::Receiver<SessionCommand>                  │
│  out:  broadcast::Sender<SessionOutput>                │
│  spawned under: TaskSupervisor (named serve.session.X) │
└────────┬───────────────────────────────────────────────┘
         │ registered in
┌────────▼───────────────────────────────────────────────┐
│  LiveSessionRegistry  (serve mode, zeph-core/src/)     │
│  parking_lot::Mutex<HashMap<SessionId, ActorHandle>>   │
│  bookkeeping only — never held across .await           │
└───────────────────────────────────────────────────────-┘
```

**Single-writer guarantee:** Each conversation-session has exactly one `SessionActor` task at any time. All appends to `events.jsonl` for a session flow through that actor's task. This guarantee is the precondition for INV-SP-2 (torn-append truncation) — if any code path bypasses the actor to write the log directly, INV-SP-2 breaks.

**Relationship to `zeph-durable` (spec-064):** `zeph-session` deliberately mirrors the append-only journal design of `zeph-durable` — sequential event ordering, a bounded-buffer replay cursor, and a single-writer actor model. The two crates are kept separate because `zeph-durable` operates at the task/step level and enforces effect idempotency; `zeph-session` operates at the conversation/context level and records semantic history. `zeph-durable`'s INV-1 constraint (`zeph-durable` MUST NOT depend on agent types such as `Message` or `MessagePart`) is the architectural reason for the separation. If the journal primitive is ever extracted into a shared library, it belongs in `zeph-common`, not in either crate. Any evolution of the journal append/replay pattern in one crate should be reviewed against the other to keep them in sync.

---

## 4. Event Log Format

### 4.1 On-Disk Layout

```
<data_dir>/<session_id>/events.jsonl   # append-only event log (source of truth)
<data_dir>/<session_id>/blobs/<hash>   # referenced image/audio blobs (by content hash)
```

`<data_dir>` defaults to `.zeph/sessions/` (sibling of `memory.sqlite_path`'s parent directory). Configurable via `[session] data_dir`.

File permissions: `0o600` on creation (owner read/write only), mirroring the db pool `set_permissions` pattern.

### 4.2 `SessionEventEnvelope` Schema

Every line in `events.jsonl` is one JSON-encoded `SessionEventEnvelope`, terminated by `\n`:

```
{
  "seq":        u64,          // monotonic, gap-free, per-session, starting at 0
  "ts_ms":      i64,          // wall-clock milliseconds (UTC)
  "turn_id":    u64 | null,   // groups events within one agent turn
  "parent_seq": u64 | null,   // fork provenance: set only on first event of a forked child log
  "kind":       SessionEvent  // tagged enum (see 4.3)
}
```

`seq` is the source of truth for ordering. `ts_ms` is informational only (may jump on clock corrections). Gaps in `seq` indicate truncated torn-writes (INV-SP-2).

### 4.3 `SessionEvent` Tagged Enum

```
SessionStarted {
  session_id:    String,                       // UUID v4
  cwd:           String,
  provider_name: String,
  model:         String,
  forked_from:   [String, u64] | null          // [parent_session_id, parent_seq_at_fork]
}

UserMessage {
  text:       String,
  image_refs: [String]                         // content-hash refs into blobs/
}

AssistantMessage {
  parts: [MessagePart]                         // reuses zeph_llm::provider::MessagePart
}

ToolCall {
  id:    String,
  name:  String,
  input: Object
}

ToolResult {
  id:          String,
  name:        String,
  output:      String,
  is_error:    bool,
  duration_ms: u64
}

Condensation {
  replaced_seq_range: [u64, u64],              // [inclusive, inclusive]
  summary:            AnchoredSummary,         // reuses zeph_common::memory::AnchoredSummary
  tokens_before:      u32,
  tokens_after:       u32
}

Compaction {
  tier:          CompactionTier,
  cleared_count: u32,
  summary:       AnchoredSummary | null
}

ForkPoint {
  new_session_id: String                       // child session minted from this point
}

ModelChanged {
  provider_name: String,
  model:         String
}

SessionEnded {
  reason: String                               // "user_quit" | "idle_ttl" | "shutdown" | "error"
}
```

**Reuse constraint:** `MessagePart` and `AnchoredSummary` are imported from existing crates. The `zeph-session` crate MUST NOT redefine them.

**Blob references:** Images and audio referenced in `UserMessage.image_refs` are stored as `<session_id>/blobs/<sha256_hex>`. When a session is deleted, its `blobs/` directory is removed with it (orphan-blob GC). Cross-session blob sharing is not supported in MVP.

**Encryption:** Session logs are NOT AEAD-encrypted by default (unlike `zeph-durable`). The data lives at user-controlled `data_dir` with `0o600` permissions. Opt-in encryption (`[session] encrypt = true`) using the `zeph-durable` cipher module pattern is deferred to a post-MVP implementation.

---

## 5. Session Store (Metadata Index)

### 5.1 Migration 105 — `acp_sessions` Column Additions

Migration `105_session_persistence.sql` is required for **both** the SQLite and PostgreSQL migration sets (`crates/zeph-db/migrations/sqlite/` and `crates/zeph-db/migrations/postgres/`). The two files are NOT byte-identical (SQLite uses `TEXT datetime('now')` defaults; PostgreSQL uses timestamp types).

**Existing columns (already present — do NOT add):**
- `id TEXT PRIMARY KEY` (migration 013)
- `created_at TEXT` / `updated_at TEXT` (migration 013)
- `conversation_id INTEGER` / `BIGINT` (migration 026)
- `title TEXT` (migration 016)

**New columns to ADD in migration 105:**

```sql
-- SQLite 105_session_persistence.sql
ALTER TABLE acp_sessions ADD COLUMN last_seq            INTEGER NOT NULL DEFAULT 0;
ALTER TABLE acp_sessions ADD COLUMN event_count         INTEGER NOT NULL DEFAULT 0;
ALTER TABLE acp_sessions ADD COLUMN forked_from         TEXT;
ALTER TABLE acp_sessions ADD COLUMN forked_at_seq       INTEGER;
ALTER TABLE acp_sessions ADD COLUMN status              TEXT NOT NULL DEFAULT 'idle'
                                   CHECK(status IN ('active', 'idle', 'archived'));
ALTER TABLE acp_sessions ADD COLUMN last_condensed_seq  INTEGER NOT NULL DEFAULT 0;

-- Unique index on conversation_id (SQLite cannot ALTER ADD UNIQUE; use CREATE UNIQUE INDEX)
-- Permits multiple NULLs (legacy rows without a conversation link)
CREATE UNIQUE INDEX IF NOT EXISTS idx_acp_sessions_conversation_id
    ON acp_sessions(conversation_id)
    WHERE conversation_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_acp_sessions_status      ON acp_sessions(status);
CREATE INDEX IF NOT EXISTS idx_acp_sessions_updated     ON acp_sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_acp_sessions_forked_from ON acp_sessions(forked_from);
```

```sql
-- PostgreSQL 105_session_persistence.sql
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS last_seq            BIGINT NOT NULL DEFAULT 0;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS event_count         BIGINT NOT NULL DEFAULT 0;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS forked_from         TEXT;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS forked_at_seq       BIGINT;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS status              TEXT NOT NULL DEFAULT 'idle'
                                              CHECK(status IN ('active', 'idle', 'archived'));
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS last_condensed_seq  BIGINT NOT NULL DEFAULT 0;

CREATE UNIQUE INDEX IF NOT EXISTS idx_acp_sessions_conversation_id
    ON acp_sessions(conversation_id)
    WHERE conversation_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_acp_sessions_status      ON acp_sessions(status);
CREATE INDEX IF NOT EXISTS idx_acp_sessions_updated     ON acp_sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_acp_sessions_forked_from ON acp_sessions(forked_from);
```

### 5.2 `SessionId` ↔ `ConversationId` Bijection

Each `SessionId` (UUID string) maps to at most one `ConversationId` (i64) and vice versa. This is enforced by the `UNIQUE` index on `acp_sessions(conversation_id WHERE NOT NULL)` added in migration 105.

Fork creates a NEW `SessionId` AND a NEW `ConversationId` (via the existing `fork_conversation` path at `crates/zeph-acp/src/agent/mod.rs:1750`), preserving the bijection. The bijection enforcement must be validated for both SQLite and PostgreSQL backends.

### 5.3 `SessionStore` API

`SessionStore` is the `zeph-session` crate's interface over `acp_sessions`. Required operations:

- `create(session_id, cwd, provider_name, model) -> Result<()>` — insert new row with `status='active'`
- `update_seq(session_id, last_seq, event_count) -> Result<()>` — called after every turn flush (INV-SP-1)
- `set_status(session_id, status) -> Result<()>`
- `set_condensed_seq(session_id, last_condensed_seq) -> Result<()>`
- `get(session_id) -> Result<Option<SessionMetadata>>`
- `list(filter: SessionFilter) -> Result<Vec<SessionMetadata>>`
- `record_fork(src_id, new_id, forked_at_seq) -> Result<()>` — sets `forked_from` + `forked_at_seq` on child, appends `ForkPoint` event to parent log
- `delete(session_id) -> Result<()>` — removes row + event log directory + blobs

---

## 6. Replay Engine

### 6.1 API

```
ReplayEngine::replay(
    session_id: &SessionId,
    up_to:      Option<u64>,     // exclusive upper bound; None = full replay (resume)
) -> Result<ReconstructedState>

struct ReconstructedState {
    messages:      Vec<Message>,    // agent's MessageState ready for hydration
    last_seq:      u64,
    conversation_id: Option<i64>,
    provider_name: String,
    model:         String,
    cwd:           String,
}
```

### 6.2 Replay Algorithm

1. Open `events.jsonl`, apply INV-SP-2 (truncate partial trailing line), get validated max `seq`.
2. If `acp_sessions.last_seq` < validated max `seq`, apply INV-SP-3 (projection reconcile) before replay.
3. Stream-parse lines into a bounded buffer (≤ 100 events in memory at once, mirroring `ReplayCursor` segment approach in `zeph-durable`).
4. Fold each event into `Vec<Message>`:
   - `UserMessage` → push `Message::User`.
   - `AssistantMessage` → push `Message::Assistant`.
   - `ToolCall` → append `ToolUse` part to the pending assistant message.
   - `ToolResult` → append matching `ToolResult` part; **no executor call** (A3: replay never re-executes tools).
   - `Condensation` → replace the `replaced_seq_range` with a single condensation-summary message.
   - `Compaction` → apply the same prune/summary the live run applied (use the recorded `summary`).
   - `ModelChanged`, `SessionStarted`, `ForkPoint`, `SessionEnded` → update metadata fields; no message emitted.
5. Stop at `up_to` (exclusive) for fork; stop at EOF for resume.
6. Return `ReconstructedState`.

**Implementation note (#5445):** step 3's bounded buffer is now implemented as described —
`ReplayEngine::replay` reads via `SessionEventLog::read_chunked` (`crates/zeph-session/src/log.rs`),
which yields parsed envelopes in chunks of ≤ 100 at a time instead of materializing the whole
file into one `Vec` first. Step 4's fold logic is factored into a shared `fold_step` helper
(`crates/zeph-session/src/replay.rs`) applied incrementally per chunk. Callers that genuinely
need the whole-file `Vec` (`ForkEngine::fork`, which copies raw lines to the child log, and
`ReplayEngine::fold` itself for callers that already hold the events, e.g. `llm_condenser.rs`)
are unaffected — they still use `SessionEventLog::read_all` and the `Vec`-based `fold` entry
point, which remain unchanged.

**Determinism guarantee:** Replay never calls the LLM or tool executors. `Condensation` and `Compaction` events record their outputs; replay folds those recorded outputs identically. The reconstructed context is byte-identical to what the live agent had at that `seq`. This is the primary correctness guarantee for #3102.

**Resume** = `replay(id, None)` → hydrate `Agent`'s `MessageState` from `ReconstructedState`, register in `LiveSessionRegistry`, continue appending at `last_seq + 1`.

---

## 7. Fork Engine

### 7.1 API

```
ForkEngine::fork(
    src_id: &SessionId,
    at_seq: u64,
) -> Result<SessionId>
```

### 7.2 Fork Algorithm (MVP: Eager Copy)

1. Validate `at_seq` ≤ `acp_sessions.last_seq` for `src_id` (cannot fork into the future).
2. `ReplayEngine::replay(src_id, up_to = Some(at_seq))` to validate the cut point is internally consistent.
3. Allocate `new_id` (UUID v4, matching ACP convention at `mod.rs:1747`).
4. Create `<data_dir>/<new_id>/` directory with `0o700` permissions.
5. Copy JSONL lines `seq ∈ [0, at_seq)` from parent log into child's `events.jsonl`. Write a `SessionStarted{ forked_from: (src_id, at_seq) }` as the first line (seq = 0 in child log, or as a header event).
6. Copy referenced blobs: for any `UserMessage.image_refs` in the copied range, hard-link (or copy) blobs into the child's `blobs/` directory.
7. Insert new `acp_sessions` row via `SessionStore::record_fork(src_id, new_id, at_seq)`.
8. Append `ForkPoint{ new_session_id: new_id }` to the **parent** log (non-destructive provenance record).
9. Backfill the child's `conversations`/`messages` projection from the replayed state.
10. Return `new_id`.

**Fork provenance semantics:** `forked_at_seq` in the child's `acp_sessions` row refers to the parent's log at fork time. After either side independently condenses the shared prefix, seq numbers diverge. `forked_at_seq` is historical metadata only; replay uses the child's self-contained log exclusively. This makes the eager-copy choice robust to condensation.

**Copy-on-write (CoW) optimization** is explicitly deferred to P2 (post-MVP). CoW requires solving shared-prefix condensation provenance, which is non-trivial. Implementing it prematurely would break the determinism guarantee.

---

## 8. Condensation Policy

### 8.1 Distinction from Compaction

| Dimension | Compaction (existing) | Condensation (new) |
|-----------|----------------------|--------------------|
| Scope | In-memory, live working set | Event-log level, durable |
| Trigger | Token budget (soft 70% / hard 90%) during a live turn | Reconstructed context exceeds `condense.threshold` on resume or mid-session |
| Persistence | Ephemeral — NOT persisted today | Persisted as `Condensation` event in log |
| Replay | Not replayable (ephemeral) | Replayable (recorded output is folded deterministically) |
| Owner | `zeph-context` | `zeph-session` `Condenser` trait |

When live hard-compaction fires, the agent also emits a `Compaction` event (with `tier`, `cleared_count`, `summary`) to make it replayable. The `Compaction` event is emitted by `zeph-agent-persistence` via the `SessionSink` path.

### 8.2 `Condenser` Trait

```rust
trait Condenser: Send + Sync {
    async fn should_condense(
        &self,
        log:    &ReconstructedState,
        budget: &ContextBudget,
    ) -> bool;

    async fn condense(
        &self,
        events: &[SessionEventEnvelope],
        budget: &ContextBudget,
    ) -> Result<CondensationResult, CondenseError>;
}

struct CondensationResult {
    replaced_range: (u64, u64),      // [inclusive, inclusive] seq range replaced
    summary:        AnchoredSummary,
    tokens_before:  u32,
    tokens_after:   u32,
}
```

**Default implementation:** `LlmCondenser` reuses `zeph-context::summarization::summarize_structured` and `SummarizationDeps` (DRY — must not duplicate the summarizer). Config: `condense_provider` references a `[[llm.providers]]` entry.

### 8.3 Non-Overlap Invariant (INV-SP-4)

`Condensation` and `Compaction` events share a non-overlap ledger. The condenser computes the next condensation range as `(acp_sessions.last_condensed_seq, current_log_tail_seq]`.

**INV-SP-4 (single-condensation-per-range):** A `Condensation` or `Compaction` event's `replaced_seq_range` MUST NOT overlap any prior `Condensation` or `Compaction` event's replaced range in the same log. The condenser MUST read `last_condensed_seq` from `acp_sessions` before computing the range and MUST update it atomically after emitting the event.

Violation of INV-SP-4 causes replay to fold a summary over events that are already summarized, producing a context the live agent never had, breaking the determinism guarantee.

**Live compaction vs. log condensation arbitration:**
- Live compaction (in `zeph-context`) fires during a live turn → emits `Compaction` event → updates `last_condensed_seq`.
- Log condensation (`LlmCondenser`) fires on resume when the reconstructed context budget exceeds `threshold` → operates on the range `(last_condensed_seq, replay_end_seq]` → emits `Condensation` event → updates `last_condensed_seq`.
- The two operations are mutually exclusive on any seq range by construction: the high-water mark `last_condensed_seq` prevents overlap.

---

## 9. Serve Mode (`zeph serve`)

### 9.1 Process Lifecycle

`zeph serve-sessions` is a long-running foreground process. Daemonization is delegated to the OS (systemd/launchd). The process owns one `TaskSupervisor` (per spec 039 — no raw `tokio::spawn`).

Supervised named tasks:
- `serve.http` — axum HTTP/SSE API (see §9.3)
- `serve.evict` — idle-session TTL eviction loop

**`--acp` (#5420, requires the `acp-http` feature):** runs the ACP protocol transport in-process as a **second axum HTTP listener** bound to `[acp] http_bind` (`zeph_acp::acp_router`), NOT the stdio path (`serve_connection`/`serve_stdio`). Two listeners rather than one merged `Router`, because `acp_router` already defines `/health` and `/sessions/{id}/messages` at its root — these collide with serve's own `/health` and `/sessions*` on `.merge()`, and the two `/sessions` surfaces are different data models (ACP's `SqliteStore`-backed CRUD vs serve's `LiveSessionRegistry` + file event-logs) that cannot be unified even in principle. ACP stdio is not viable as a co-tenant supervised task: `serve_stdio` has no cancellation hook and blocks reading `stdin` on a dedicated OS thread, which reads immediate EOF under a daemon's `StandardInput=null` — the combined process would silently run only the HTTP API while `serve.acp` completes instantly doing nothing.

Both listeners share ONE `SemanticMemory`/`SQLite` pool and ONE `TaskSupervisor` (`crate::acp::build_combined_deps`/`crate::acp::build_shared_core`), rather than each transport opening an independent pool on the same database file. A wildcard-aware pre-check rejects overlapping `[serve] http_addr`/`[acp] http_bind` bindings (same port, and either equal IPs or either IP unspecified `0.0.0.0`/`::`) before either listener binds. Exactly one SIGTERM/Ctrl-C handler drives a shared `CancellationToken` that both listeners' graceful shutdown observes.

### 9.2 Per-Session Actor Model

Each live conversation-session is a `SessionActor` task:

```
SessionActor owns:
  agent:  Agent<LoopbackChannel>       // exclusive &mut, never shared
  log:    Arc<SessionEventLog>         // append target
  rx:     mpsc::Receiver<SessionCommand>
  tx_out: broadcast::Sender<SessionOutput>

SessionCommand:
  Prompt { text: String, reply_to: broadcast::Sender<SessionOutput> }
  Cancel
  Shutdown

SessionOutput:
  Token(String)
  ToolEvent(ToolCall | ToolResult)
  TurnComplete
  Error(String)
```

The actor is spawned via `TaskSupervisor::spawn("serve.session.<id>", ...)`. `Agent::run(&mut self)` (`crates/zeph-core/src/agent/mod.rs:359`) takes exclusive ownership of the agent for each turn — this is compatible with the actor model because the actor task is the sole owner.

**Concurrency policy:** Same-session prompts are serialized by the mpsc mailbox FIFO (no separate turn-lock needed). Cross-session actors run in parallel (one task each). Backpressure: bounded mpsc (`serve.max_queued_prompts`, default 8); `try_send` failure returns HTTP 429 (Too Many Requests) to the caller.

### 9.3 `LiveSessionRegistry`

```
LiveSessionRegistry  (serves mode only; distinct from TUI's SessionRegistry/tabs)
= parking_lot::Mutex<HashMap<SessionId, SessionActorHandle>>

SessionActorHandle {
    tx:       mpsc::Sender<SessionCommand>,
    tx_out:   broadcast::Sender<SessionOutput>,
    last_active: Instant,
}
```

The mutex is bookkeeping only, never held across `.await`. Connect to existing session: look up handle, subscribe to broadcast, send prompts via mpsc. Connect to absent session: replay → spawn `SessionActor` → register → attach.

**Idle eviction** (`serve.evict` task): periodically scan registry; sessions with no attached broadcast receivers for `serve.session_idle_ttl_secs` → send `Shutdown`, flush log writer, mark `acp_sessions.status = 'idle'`, remove handle from registry.

**Graceful shutdown:** `TaskSupervisor::shutdown_all(timeout)` sends `Shutdown` to all actor tasks, then waits for log flush before exit.

### 9.4 HTTP/SSE API

Bearer authentication via BLAKE3 + `subtle::ConstantTimeEq` (reuse `zeph-gateway` pattern). Per-IP rate limiting. `/health` endpoint is unauthenticated.

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/sessions` | Create new session; returns `{ session_id }` |
| GET | `/sessions` | List sessions (paginated, filterable by status) |
| GET | `/sessions/:id` | Get session metadata |
| POST | `/sessions/:id/prompt` | Send prompt; streaming SSE response |
| GET | `/sessions/:id/events` | Stream all events SSE (for TUI/CLI attach) |
| POST | `/sessions/:id/fork` | Fork at optional `seq`; returns `{ new_session_id }` |
| DELETE | `/sessions/:id` | Archive/delete session |

Prompt response uses SSE with event types: `token`, `tool_call`, `tool_result`, `turn_complete`, `error`.

### 9.5 Feature Gate

`zeph serve` and the persistence CLI verbs are gated behind a `session` feature flag, independent of the existing `acp` feature. The `acp` feature continues to gate ACP protocol transports. Existing `SessionsCommand` (currently `#[cfg(feature = "acp")]`) gains an additional `#[cfg(feature = "session")]` path for the new verbs so they work without the ACP transport stack.

Recommended feature bundles update: add `session` to `desktop` and `server` bundles.

---

## 10. CLI Commands

**Extend existing `SessionsCommand`** (`src/cli.rs:888`, `SessionsCommand`). Do NOT create a parallel top-level command.

```
zeph sessions list                           # EXISTING — enrich output: title, status, event_count, forked_from
zeph sessions resume <id>                    # EXISTING — upgrade: live replay hydrate + continue (--print for old dump behavior)
zeph sessions delete <id>                    # EXISTING — also remove event-log dir + blobs
zeph sessions show <id> [--from N] [--to N] [--events]   # NEW
zeph sessions fork <id> [--at <seq>]         # NEW
zeph sessions export <id> <path.jsonl>       # NEW — write events.jsonl copy to path
zeph sessions import <path.jsonl>            # NEW — create new session from JSONL file
```

**`zeph serve`** is a new top-level `Command` variant (`Serve(ServeArgs)`) added to `src/cli.rs:306`.

**Behavior change:** `sessions resume` currently prints events to stdout. It becomes a live resume (hydrate + continue). `--print` flag retains the old dump behavior. This is a pre-1.0 breaking change; document in `CHANGELOG.md [Unreleased]`.

---

## 11. TUI Commands

New `/conv` commands in the slash-command registry (distinct from `/session` tab commands):

| Command | Description |
|---------|-------------|
| `/conv list` | Show paginated list of persisted conversation-sessions |
| `/conv resume <id>` | Replay and hydrate the named conversation into the active TUI tab |
| `/conv fork <id> [--at <seq>]` | Fork conversation at optional seq; open in new tab |
| `/conv show <id>` | Display metadata and event summary for a conversation |

Spinner rule (spec-011 TUI §3): all replay/condensation operations must show a visible status indicator:
- `Replaying conversation…` — during `ReplayEngine::replay`
- `Condensing history…` — during `LlmCondenser::condense`
- `Saving session…` — during event-log flush

---

## 12. Agent Loop Integration

### 12.1 `SessionSink`

`zeph-agent-persistence` gains a `SessionSink` that intercepts every turn result and:
1. Appends the appropriate `SessionEvent`s to the log writer (`SessionEventLog`).
2. Calls `SessionStore::update_seq(session_id, new_last_seq, new_event_count)`.
3. Calls the existing persistence path to write the `messages` SQLite projection.

Step 1 happens before steps 2 and 3, satisfying INV-SP-1.

### 12.2 `SessionId` Threading

For non-ACP channels, the `Agent` startup path mints a `SessionId` and creates an `acp_sessions` row if `[session] enabled = true`. The `SessionId` is stored in the agent's config context and passed to `SessionSink` on each turn.

The existing `SessionId` type in `zeph-common` (`044-zeph-common/spec.md`) is reused. No new `SessionId` newtype is introduced.

### 12.3 ACP Handler Delegation

The four existing ACP session handlers are thinned to delegate to `zeph-session` engines:

| Handler | Current location | New behavior |
|---------|-----------------|--------------|
| `do_load_session` | `zeph-acp/src/agent/mod.rs:1548` | Calls `ReplayEngine::replay` |
| `do_list_sessions` | `mod.rs:1642` | Calls `SessionStore::list` |
| `do_fork_session` | `mod.rs:1698` | Calls `ForkEngine::fork` |
| `do_resume_session` | `mod.rs:1809` | Calls `ReplayEngine::replay` then hydrates agent |

**Behavior-preservation requirement:** existing ACP fork/resume integration tests must pass unchanged after this delegation. The `SessionId` ↔ `ConversationId` bijection (§5.2) must be maintained.

---

## 13. Resume UX and History Visibility

### 13.1 Problem Statement

On resume, prior conversation history is loaded into the agent's LLM context (`agent.with_preloaded_messages` / `Agent::load_history`, see `crates/zeph-core/src/agent/persistence/history.rs:31-70`) but is never rendered to the user's display buffer. Default flagless startup already re-attaches to `latest_conversation_id()` (`src/runner.rs:2028`), so **every** restart silently continues the newest conversation — "continuing an interrupted session" is the common path, not an edge case. `SessionSlot::messages` starts empty (`crates/zeph-tui/src/session.rs:169-193`) and is fed only by the live `AgentEvent` stream; the CLI prints nothing at startup. The agent remembers, but the screen looks like a fresh start. This section formalizes the fix as a single `zeph-core` presentation primitive rendered per-channel, per the Multi-Model/Cross-Mode-Consistency principle — no per-channel resume logic is duplicated.

### 13.2 Scope — Display-Owning Channels Only

This is a **conditional** invariant (INV-SP-5, §14), not a universal one, and is deliberately kept out of `specs/001-system-invariants/spec.md`. It applies only to channels that own their own display buffer:

- **In scope:** `CliChannel`, `TuiChannel` — stdout / `SessionSlot::messages` are the sole transcript surface; hiding history there is a real problem.
- **Exempt:** `TelegramChannel`, `DiscordChannel`, `SlackChannel` — these render server-side scrollback in the client itself; prior history is never hidden, so a banner (or an automatic `/history` re-dump) would be pure noise. See `specs/007-channels/spec.md` for the channel-parity table.
- **Exempt in v1:** ACP/IDE-embedding sessions (`zeph-acp`) — see §13.8.

### 13.3 `SessionResumeInfo` (Core Primitive)

Computed once, in `zeph-core`, at hydration time — reusing the `ReplayEngine::fold` pass that `init_session_sink` already awaits at startup (`src/runner.rs:642+`), so this is zero additional I/O on the hydration path:

```rust
struct SessionResumeInfo {
    session_id:          SessionId,
    conversation_id:     Option<i64>,
    is_resume:           bool,                    // see §13.4
    prior_message_count: usize,
    prior_turn_count:    usize,
    last_active:         Option<OffsetDateTime>,   // derived from acp_sessions.updated_at — no new data
}
```

- Does **not** carry the full message vector — expansion is pulled lazily by `/history` (§13.6), never eagerly.
- `was_interrupted` is explicitly NOT a field in v1 — see §13.10.
- `last_active` requires no new persisted data: it is the existing `acp_sessions.updated_at` column (§5.1).

### 13.4 `is_resume` Predicate (Exact Definition)

```
is_resume = reconstructed history from ReplayEngine::fold (or, when [session] enabled = false,
            the PersistenceService::load_history SQLite fallback) contains
            AT LEAST ONE non-system SessionEvent / Message
            (any UserMessage, AssistantMessage, ToolCall, or ToolResult)
```

**This MUST be evaluated against the raw reconstructed event/message stream — never against a display-filtered, visible-text-only turn count.** A session interrupted mid-tool-loop reconstructs to `[system, assistant(tool_use-only), user(tool_result)]` — no assistant text yet. The TUI's display filter skips `is_tool_use_only` assistant messages (`state.rs:492-498`); counting *display* turns would make this exact "interrupted session" case false-negative as fresh (no banner) — precisely the case the user reported. Counting raw non-system events closes this hole.

A conversation containing only the system prompt (zero user/assistant/tool events) is `is_resume = false` (fresh).

### 13.5 Resume Banner Rendering

- **CLI** (`crates/zeph-channels/src/cli.rs`): print one neutral line at startup, e.g. `↻ Resuming session (last active 2h ago) — 34 messages, 12 turns. Type /history to view.` No interrupted/clean-exit qualifier in v1 (§13.10).
- **TUI**: new `AgentEvent::ResumeBanner(SessionResumeInfo)` variant renders a **persistent** banner (not a transient status-spinner line) in the header/status area, since it must remain visible once the first prompt scrolls the transient status out of view. See `specs/011-tui/spec.md` for wiring detail. Exact placement (header vs. dedicated collapsible line) is an implementation choice, not a spec constraint.
- **Chat** (Telegram/Discord/Slack): emit nothing at startup (§13.2).
- **ACP/IDE-embedding**: emit nothing at startup in v1 (§13.8).
- **Multi-channel-single-process (`serve`):** when more than one display-owning channel attaches to the same `conversation_id`, the banner MUST fire exactly once, not once per attaching channel — ownership of the single emission belongs to whichever display-owning channel triggers the initial hydration.

### 13.6 `/history [N|all]` Command Contract

Registered in `zeph-commands` (channel-agnostic, alongside `/recap`), backed by a shared `TranscriptFormatter` (a function/small struct converting an already-bounded `Vec<Message>`/`ReconstructedState` slice into role-prefixed, tool-collapsed text). `TranscriptFormatter` is the single source of truth reused by both the CLI expand path and the TUI backfill path — neither implements its own formatting.

- **Default (`/history`, no arg):** bounded to the last `N` messages, config `[session.resume] expand_default_lines` (default 20). **The last-N slice MUST be taken before formatting/materialization** — never materialize the full reconstructed history and then trim (INV-SP-6, §14).
- **`/history all`:** an explicitly heavier operation.
  - MUST NOT synchronously format and push the full history onto a single-threaded render/input loop (violates the non-blocking render-loop contract, `CLAUDE.md` Async & Background Tasks / spec-039).
  - Implementation MUST do one of: (a) format off-thread in a supervised task (`TaskSupervisor`/`BackgroundSupervisor`) and deliver the result via `AgentEvent`, or (b) paginate (e.g. `/history next`).
  - MUST emit an explicit "showing full history (N messages) — this may take a moment" notice before the heavier path runs.
- **Availability:** `/history` remains manually invokable on **every** channel, including channels exempt from the automatic banner (§13.2, §13.8) — only the startup banner is scoped by display-ownership, not the command itself.
- **Chat channel output size:** if `/history` (in particular `/history all`) is invoked in a chat channel, the formatted output MUST chunk to the channel's message-size limit (e.g. Telegram's 4096-character limit) rather than silently truncating or erroring; alternatively cap with an explicit "truncated — use CLI/TUI for full history" note.
- **Auth posture:** `/history` does not require the elevated trust level (`requires_auth = true`) that history-mutating commands like `/clear`/`/reset` require, because the chat-channel availability requirement above forces it — a `requires_auth = true` command cannot dispatch on an untrusted channel at all. This means any channel-allowed user (i.e., anyone who has passed that channel's own `allowed_users`/guest gate) can read the full shared conversation transcript via `/history all`, including raw tool-result content, on any channel where `/history` is available. This is the intended v1 trust posture, not an oversight: reading history is treated as no more sensitive than any other bot interaction on an already-allowed channel. A finer-grained gate (e.g. an operator-only allowlist distinct from `allowed_users`) is out of scope for v1.

### 13.7 TUI Specialization

See `specs/011-tui/spec.md` for the full cross-reference. Summary:

- `/history` backfills `SessionSlot::messages` via the existing-but-dead `App::load_history` (`crates/zeph-tui/src/app/state.rs:476`), receiving only the bounded slice already produced by `TranscriptFormatter` — never the full log.
- The backfill path MUST be split from `input_history` (readline up-arrow recall) population — `App::load_history` currently pushes every prior user message into `input_history` (`state.rs:501-505`); a display-only variant/flag prevents old messages from flooding recall.
- The dead `SessionBrowser`/`session:history` command (dispatched at `command.rs:343`, swallowed by `_ => continue` in `src/tui_bridge.rs:554`) is wired to the same bounded action and rebound to key `H`.
- Backfilling also makes Ctrl+F transcript search and PageUp/Home/End scrollback work over the backfilled range, since both already operate on `SessionSlot::messages` / `app.visible_messages()`.

### 13.8 ACP / IDE-Embedding Classification

The ACP/IDE-embedding surface (`src/acp.rs`, which also calls `agent.load_history`) is explicitly classified rather than left silent:

**Decision: exempt, like chat, for v1.** An IDE client embedding Zeph via ACP is expected to own and persist its own transcript/thread view across reconnects — the same reasoning that exempts Telegram/Discord/Slack (§13.2). The automatic resume banner (§13.5) MUST NOT fire for ACP-owned sessions in v1. `/history` remains available as a manual command over ACP if the transport surfaces slash-command dispatch (§13.6), unaffected by this exemption.

This is an explicit decision, not a silent omission — it prevents the same "works in CLI, silently skipped elsewhere" ambiguity the display-owning classification exists to close. Revisit in v2 if an IDE client reports a need for a Zeph-rendered resume indicator (track alongside the DR-2 interrupted-detection follow-up, §13.10).

### 13.9 Edge Cases

- **Forked sessions (`ForkEngine::fork`):** a fresh fork inherits a folded history and is treated as **resume**, not fresh — the banner fires if the inherited conversation is non-empty per §13.4. `SessionResumeInfo` is computed from the post-fork reconstructed state, consistent with any other resume.
- **"Non-empty prior conversation" definition:** see §13.4 — raw reconstructed non-system event count, not display-filtered turns.
- **`/history all` in chat:** see §13.6 chat output size chunking.
- **Multiple display-owning channels in one `serve` process:** see §13.5 single-emission rule. Cross-*process* concurrency for the same session is already prevented by the existing flock (`crates/zeph-session/src/log.rs:43,472`), so this edge case is intra-process only (e.g. CLI attach + TUI attach to the same live `conversation_id`).

### 13.10 v1 Non-Goals — Deferred to v2

**Interrupted-vs-clean-exit detection (formerly DR-2) is explicitly OUT OF SCOPE for this spec revision.** v1 ships a neutral "Resuming session" banner with no interrupted/clean qualifier — this fully resolves the reported complaint ("looks like starting from scratch, don't see history"). Reasons for deferral, recorded so the omission is not silently repeated in review:

- `SessionStatus` is `Active | Idle | Archived` (`crates/zeph-session/src/store.rs:24-31`) — there is **no `Closed` variant**. Adding one requires a new enum arm, `as_str`/`from_str` updates, and a migration — not a small additive change.
- `SessionStore::set_status` is **never called in production** today (only in `store.rs` tests). Wiring a clean-exit marker would require touching every exit path (CLI Ctrl-D, TUI quit, ACP, daemon, chat) — missing any one mislabels a clean exit as "interrupted."
- **v2 constraint to honor when this is built:** the already-merged stale-session-lock signal (#6378) is *also* crash evidence (a stale/auto-released flock at startup). A future clean-exit marker and the lock-staleness signal are two signals for one fact — they MUST be reconciled to a single source of truth, not left free to disagree.

This deferral has zero v1 impact: `SessionResumeInfo` in v1 carries no `was_interrupted` field, and the banner text carries no interrupted/clean language.

---

## 14. Key Invariants

### INV-SP-1 — Log-first ordering (source of truth)
A turn's `SessionEvent`s are appended to `events.jsonl` and flushed (via the log writer actor) **before** the SQLite `messages` projection or `acp_sessions.last_seq` are updated. The projection never leads the log. A crash between the two leaves the log ahead; INV-SP-3 reconciles on next open.

### INV-SP-2 — Torn-append truncation on open
On opening `events.jsonl`, the reader validates each line. A final partial/garbled line (interrupted append, no terminating `\n`, or invalid JSON) is **truncated** (file rewound to the last valid `\n`). Earlier lines are durable (appends are single `write` + `fsync` per event; OS guarantees ordering within a file). A torn write is thus always the last line. Recovery discards at most one in-flight event, which was never acknowledged to the caller.

**Precondition:** INV-SP-2 holds **only** because the actor model (§9.2) guarantees a single writer per session log. Any code path that bypasses the actor to append to the log directly violates this precondition and breaks the guarantee.

### INV-SP-3 — Projection reconcile-from-log on open
When a session is opened (resume, fork, serve attach), if `acp_sessions.last_seq` < the event log's validated max `seq` (post-INV-SP-2 truncation), the projection is **rebuilt forward** from the log for the missing range: replay events `(last_seq, max_seq]`, write the corresponding `messages` rows, update `last_seq`. The event log is authoritative; SQLite is always a derivable projection.

### INV-SP-4 — Condensation non-overlap (determinism preservation)
A `Condensation` or `Compaction` event's `replaced_seq_range` MUST NOT overlap any prior `Condensation` or `Compaction` event's replaced range in the same session log. The condenser reads `acp_sessions.last_condensed_seq` before computing the range and updates it atomically after emitting the event.

### INV-D1 — No fourth "session" concept (terminology stability)
No code path shall introduce a new "session" concept without updating the terminology table in §2 and resolving naming collisions.

### INV-D2 — Single writer per session log
Each conversation-session's `events.jsonl` is written exclusively by its `SessionActor` task (or, for non-serve mode, by the single active agent process). No concurrent writes from multiple tasks or processes.

### INV-SP-5 — Resume Visibility (display-owning channels only)
A channel that owns its own display buffer (`CliChannel`, `TuiChannel`) MUST NOT silently hide prior conversation history when attaching to a non-empty prior conversation-session (§13.4 `is_resume`) on startup: it MUST render a resume indicator (§13.5) and MUST expose a way to view prior messages (`/history`, §13.6). Channels that render their own server-side scrollback (Telegram, Discord, Slack) and ACP/IDE-embedding sessions in v1 (§13.8) are exempt. This invariant is intentionally scoped and MUST NOT be elevated to `specs/001-system-invariants/spec.md` as a universal MUST.

### INV-SP-6 — Non-Blocking History Expansion
`/history` MUST bound its default result to the last N messages (config `expand_default_lines`) before formatting/materialization — never materialize-then-trim. `/history all` MUST NOT synchronously push the full history onto a single-threaded render/input loop; it MUST format off-thread under a supervisor or paginate, per the non-blocking render-loop contract (`CLAUDE.md` Async & Background Tasks, spec-039).

---

## 15. Related Specifications

| Spec | Relationship |
|------|-------------|
| `specs/064-durable-execution/` | **Primary design precedent.** `zeph-session` mirrors the append-only journal, bounded-buffer replay cursor, and single-writer actor model established there. Key difference: durable execution records step/effect idempotency at the tool level; session persistence records conversation semantics at the message level. The two journals are independent and reference each other only by opaque IDs. |
| `specs/013-acp/` | `zeph-session` generalizes ACP session management (`do_fork_session`, `do_resume_session`, `do_load_session`, `do_list_sessions`) below the ACP layer so all channels share the same engine. ACP handlers become thin callers of `zeph-session` engines. |
| `specs/022-zeph-context/` | `zeph-context::summarization::summarize_structured` is reused by `LlmCondenser` (DRY). The `CompactionState` machine in `zeph-context` triggers `Compaction` event emission (via `SessionSink`) when live hard-compaction fires. |
| `specs/039-background-task-supervisor/` | `SessionActor` tasks are spawned via `TaskSupervisor::spawn` per spec-039 invariants. No raw `tokio::spawn` for session actors. |
| `specs/044-zeph-common/` | `SessionId` (UUID v4 newtype) already defined in `zeph-common` and reused without modification. |
| `specs/032-database-abstraction/` | Migration 105 (SQLite + PostgreSQL) follows the dual-migration-set pattern established in spec-032. |
| `specs/007-channels/spec.md` | Channel-parity note for INV-SP-5: display-owning channels (CLI, TUI) render the resume banner; chat channels (Telegram/Discord/Slack) are exempt. |
| `specs/011-tui/spec.md` | TUI specialization for §13: banner placement, bounded `/history` backfill via `App::load_history`, `SessionBrowser` wiring, non-blocking `/history all` per INV-SP-6. |

---

## 16. NEVER

- **NEVER** write to a session's `events.jsonl` from outside its `SessionActor` task (or the single active agent) — this breaks INV-SP-2 and INV-D2.
- **NEVER** call the tool executor during replay — replay reads recorded `ToolResult` outputs, never re-executes tools.
- **NEVER** introduce a second `sessions` table alongside `acp_sessions` — D1 explicitly prohibits this.
- **NEVER** introduce a new `SessionId` newtype — the existing one in `zeph-common` is reused.
- **NEVER** emit a `Condensation` event whose `replaced_seq_range` overlaps a prior `Condensation`/`Compaction` range (INV-SP-4).
- **NEVER** hold a `parking_lot::Mutex` guard across `.await` in `LiveSessionRegistry` — bookkeeping only.
- **NEVER** use raw `tokio::spawn` for session actors — always use `TaskSupervisor::spawn` (per spec 039).
- **NEVER** implement CoW fork optimization without resolving shared-prefix condensation provenance first.
- **NEVER** store auth tokens inline in config — use vault resolution (`[serve] auth_token` is vault-only).
- **NEVER** make `zeph-session` depend on `zeph-durable` directly (or vice versa). The two crates record different concerns at different abstraction levels. If shared journal primitives are ever extracted into a reusable library, they belong in `zeph-common`, not in either crate.
- **NEVER** silently skip the resume banner on a display-owning channel (CLI, TUI) when resuming a non-empty prior conversation-session — INV-SP-5.
- **NEVER** compute the `is_resume` predicate against a display-filtered/visible-text turn count — always evaluate against the raw reconstructed event/message stream (INV-SP-5, §13.4).
- **NEVER** materialize the full reconstructed history in `/history` before bounding to last-N — bound before formatting (INV-SP-6).
- **NEVER** push backfilled display history into `input_history` — display-only hydration is a separate code path from readline recall population (§13.7).
- **NEVER** elevate the resume-visibility invariant (INV-SP-5) to `specs/001-system-invariants/spec.md` — it is conditional on display ownership, not universal.

---

## 17. Affected Subsystems

| Crate | Change level | What changes |
|-------|-------------|--------------|
| **zeph-session** (NEW) | New crate | `SessionEvent`, `SessionEventEnvelope`, `SessionEventLog`, `SessionStore`, `ReplayEngine`, `ForkEngine`, `Condenser` trait + `LlmCondenser`. Does NOT depend on `zeph-durable`. The writer-actor pattern in `zeph-session` is structurally similar to `zeph-durable`'s journal but not a code reuse of it: the two systems differ on storage engine (JSONL files vs SQLite), replay semantics (linear fold into `Vec<Message>` vs per-`StepId` idempotency arbitration), and payload format (domain-typed readable JSON vs AEAD-sealed opaque bytes). The real overlap is ~80–120 LOC of generic "append/read/actor" idiom, not shared machinery. |
| `zeph-core` | Medium | `SessionActor` wrapper (owns `Agent`, mpsc-in/broadcast-out) for serve mode; agent emits `SessionEvent` per turn; computes `SessionResumeInfo` at hydration and emits `AgentEvent::ResumeBanner` (§13.3) |
| `zeph-common` | None (optional small addition) | `SessionId` already present (spec 044); no changes required. Optional P2 addition: ~40–60 LOC JSONL line-framing + torn-tail-truncation helper (INV-SP-2 logic), storage-agnostic, usable by any future JSONL consumer. Not a dependency of `zeph-session`'s acceptance. |
| `zeph-config` | Small | `[session]` + `[serve]` config structs; `condense_provider` ProviderName field; new `[session.resume]` section (`show_banner`, `auto_expand`, `expand_default_lines`, `show_recap`, §13/§18); migration step 106+ for `--migrate-config` |
| `zeph-db` | Small | Migration 105 (SQLite + PostgreSQL) alters `acp_sessions` |
| `zeph-context` | Small | Expose `summarize_structured`/`SummarizationDeps` for reuse by `LlmCondenser`; emit `Compaction` event hook |
| `zeph-agent-persistence` | Small | `SessionSink` dual-write (log-first per INV-SP-1) |
| `zeph-acp` | Medium | `do_fork/resume/load/list_session` delegate to `zeph-session` engines; behavior-preservation tests required; ACP resume banner exempt in v1 (§13.8) |
| `zeph-channels` | Small | `CliChannel` prints the neutral resume banner line at startup (§13.5); chat channels (`TelegramChannel`/`DiscordChannel`/`SlackChannel`) emit nothing |
| `zeph-commands` | Small | New `/history [N|all]` command (§13.6) + shared `TranscriptFormatter`, channel-agnostic like `/recap` |
| `zeph-tui` | Small | `/conv` commands; replay/condensation spinners; wires the dead `App::load_history`/`SessionBrowser` to bounded `/history` backfill (display-only, split from `input_history`); renders `AgentEvent::ResumeBanner` (§13.5, §13.7) |
| `zeph-gateway` | Reuse only | Auth/rate-limit patterns reused for `serve.http`; crate not modified |
| `src/` (binary) | Medium | Extend `SessionsCommand` (+show/fork/export/import); add `Serve(ServeArgs)` to `Command`; `LiveSessionRegistry` + serve actor wiring under `TaskSupervisor` |

---

## 18. Configuration

```toml
[session]
enabled     = true
data_dir    = ".zeph/sessions"       # default: sibling of memory.sqlite_path parent
encrypt     = false                  # opt-in AEAD; deferred to post-MVP
max_event_log_mb = 256               # rotate/condense trigger guard

[session.condense]
condense_provider = "fast"           # ProviderName → [[llm.providers]]; empty = default provider
threshold         = 0.85             # fraction of context budget that triggers condensation
keep_recent       = 20               # minimum number of recent events to preserve after condensation

[session.resume]
show_banner         = true           # display-owning channels only (CLI/TUI); no effect on chat/ACP (§13.2, §13.8)
auto_expand         = false          # if true, always render full history on startup instead of collapsed banner
expand_default_lines = 20            # bound for /history with no argument (§13.6)
show_recap          = false          # opt-in: fold session.recap summary into the resume banner

[serve]
http_addr             = "127.0.0.1:7878"
acp                   = false
auth_token            = ""           # vault-resolved; never stored inline
require_auth          = true
max_sessions          = 64
session_idle_ttl_secs = 3600
max_queued_prompts    = 8            # bounded mpsc size per session; 429 on overflow
```

---

## 19. Migration Path

**New installs:** `[session] enabled = true` → event logs from first turn.

**Existing installs:** `--migrate-config` adds `[session]` and `[serve]` sections. A one-shot DB migration (105) alters `acp_sessions`. Event logs are NOT retroactively synthesized from old `messages` rows (lossy — no tool-call/result granularity available). Instead: legacy conversations resume via the projection path (SQLite history load) and receive an event log lazily — the first resume writes a `SessionStarted` + a `Condensation`-style "imported history" summary event, after which new turns append normally. Old sessions cannot be forked at arbitrary historical `seq`, only at the import boundary (this constraint is documented in `sessions show --events` output).

**Non-ACP sessions** (CLI, TUI, Telegram) that predate migration 105 do not have `acp_sessions` rows. They receive new rows on first turn after the migration if `[session] enabled = true`.

**`--migrate-config` also gains a step** seeding `[session.resume]` defaults (§13, §18) for existing configs — verify the current max `--migrate-config` step at implementation time and add this as the next free step (a prior cycle collided on step numbering; re-check rather than assume a specific number).

---

## 20. Open Questions (Resolved and Deferred)

| ID | Question | Resolution |
|----|---------|-----------|
| OQ-A | Store `last_condensed_seq` vs derive-by-scan? | **Store it** as a column in `acp_sessions` (cheap, avoids full scan on every turn) |
| OQ-B | Introduce `session` feature decoupled from `acp`? | **Yes** — `session` feature gates `SessionsCommand` persistence verbs and `zeph serve`; `acp` gates ACP protocol only |
| OQ-C | Serve prompt backpressure: queue vs reject-while-busy? | **Queue** with bounded mpsc (`max_queued_prompts = 8`); 429 on overflow; configurable |
| OQ-D | Encrypt session logs by default? | **No** — opt-in only (`[session] encrypt = false`); 0o600 file perms are the default protection |
| OQ-E | CoW fork for large sessions? | **Deferred to P2** — eager copy is simpler and self-contained for MVP |
| OQ-F | Blob GC when session deleted? | **Synchronous** — delete `blobs/` dir with the session; cross-session blob sharing not supported |
| OQ-G | `acp_sessions` rename to `conversation_sessions`? | **Deferred to post-1.0 P4** cosmetic migration |
| OQ-H | Interrupted-vs-clean-exit detection (DR-2) for the resume banner? | **Deferred to v2** — v1 ships a neutral banner only; when built, MUST reconcile with the #6378 stale-lock signal (§13.10) |
| OQ-I | TUI resume banner placement: header vs. dedicated collapsible line above input? | **Implementation choice, not a spec constraint** — either satisfies §13.5; low risk |
| OQ-J | Does the resume banner fire for ACP/IDE-embedding sessions in v1? | **No** — exempt like chat in v1 (§13.8); revisit in v2 if an IDE client requests it |

---

## 21. Acceptance Criteria

All criteria are observable and testable.

| ID | Criterion | How to verify |
|----|-----------|---------------|
| AC-1 | A CLI session survives a `SIGKILL` and resumes with no message loss | Kill the process mid-turn; `zeph sessions resume <id>`; verify last complete turn is present |
| AC-2 | `ReplayEngine::replay` produces byte-identical `MessageState` for the same log | Replay the same log twice; assert `messages == messages` |
| AC-3 | A fork at `seq=N` produces a child that replays correctly up to `seq=N` | `sessions fork <id> --at N`; replay child; verify exactly N events |
| AC-4 | INV-SP-2: a truncated last line is dropped cleanly | Truncate last line of `events.jsonl`; open session; verify no panic, `last_seq` is N-1 |
| AC-5 | INV-SP-3: projection reconciles after crash between log and SQLite writes | Write event to log; kill before projection write; resume; verify projection matches log |
| AC-6 | INV-SP-4: condensation range does not overlap prior condensation | Condense twice; verify `replaced_seq_range` of second starts after first ends |
| AC-7 | `zeph serve` handles two simultaneous connections to the same session | Two clients send prompts concurrently; verify FIFO ordering, no panics, both receive responses |
| AC-8 | Migration 105 runs idempotently on SQLite and PostgreSQL | Apply 105 twice; no error, `acp_sessions` schema is unchanged |
| AC-9 | ACP `do_fork_session` produces the same result as `ForkEngine::fork` directly | Fork via ACP and directly; compare child logs |
| AC-10 | `/conv resume <id>` in TUI shows "Replaying conversation…" spinner | Trigger resume; observe spinner in TUI status bar |
| AC-11 | `zeph sessions export/import` round-trips a session | Export to file; import; compare event counts and `sessions list` output |
| AC-12 | `serve.session_idle_ttl_secs` eviction fires and marks session `idle` | Attach then detach; wait TTL; verify `acp_sessions.status = 'idle'` |
| AC-13 | `sessions resume` with `--print` dumps events to stdout (backward compat) | `zeph sessions resume <id> --print`; verify JSONL output, no agent started |
| AC-14 | Resuming a non-empty prior conversation via CLI prints a neutral "Resuming session" banner with turn count and last-active | Start CLI against a DB with ≥1 prior turn; verify the banner line is printed at startup with no interrupted/clean qualifier |
| AC-15 | Resuming a non-empty prior conversation via TUI renders a persistent `AgentEvent::ResumeBanner` | Start TUI against non-empty history; verify the banner is rendered in the header/status area within the same render frame as the first prompt |
| AC-16 | A fresh conversation (system-prompt-only, zero user/assistant/tool events) shows NO resume banner in CLI or TUI | Start against a brand-new conversation; verify no banner is printed/rendered |
| AC-17 | A conversation interrupted mid-tool-loop (`[system, assistant(tool_use-only), user(tool_result)]`, no assistant text) still evaluates `is_resume = true` | Construct such a log; resume; verify the banner fires (regression test for the M-REV2-2 predicate fix, §13.4) |
| AC-18 | `/history` with no argument returns at most `expand_default_lines` most-recent messages, sliced before formatting | Invoke `/history` on a 500-turn conversation; verify only the last N messages are ever passed to `TranscriptFormatter` |
| AC-19 | `/history all` on a large conversation does not stall the TUI render loop | Invoke `/history all` on a 2000+ message history in TUI; verify the render loop keeps processing input/frames while formatting proceeds off-thread (or paginated), and a "may take a moment" notice is shown first |
| AC-20 | TUI `/history` backfill does not pollute `input_history`/up-arrow recall | Invoke `/history`; press up-arrow in the input composer; verify only genuinely-typed prior inputs appear, never backfilled message text |
| AC-21 | Chat channels (Telegram/Discord/Slack) never emit the resume banner | Resume a non-empty conversation via Telegram; verify no banner-equivalent message is sent |
| AC-22 | `/history all` invoked in a chat channel chunks output to the channel's message-size limit | Invoke `/history all` in Telegram on a large conversation; verify output arrives as multiple messages under the 4096-character limit, or as an explicit truncation notice |
| AC-23 | A forked session (`ForkEngine::fork`) with non-empty inherited history shows the resume banner on first attach to the new session | Fork a non-empty conversation; open/attach the child; verify the banner fires |
| AC-24 | In `serve` mode, when multiple display-owning channels attach to the same `conversation_id`, the resume banner fires exactly once | Attach CLI, then TUI, to the same live session; verify only one channel renders/prints the banner |

---

## 22. Implementation Roadmap

See `plan.md` for the full phased plan. Summary:

- **P0 — Foundation:** `zeph-session` crate scaffold; `SessionEvent` schema; `SessionEventLog` (JSONL writer); migration 105; `SessionStore`. Unit-tested in isolation.
- **P1 — Replay + emit:** `ReplayEngine`; agent loop `SessionSink` dual-write; resume path; CLI `sessions list/show/resume`.
- **P2 — Fork + condensation:** `ForkEngine` (eager copy); `Condenser`/`LlmCondenser`; `Compaction`/`Condensation` events; ACP delegation; CLI `sessions fork/export/import`.
- **P3 — Serve:** `SessionActor`; `LiveSessionRegistry`; `zeph serve` under `TaskSupervisor`; HTTP+SSE; ACP multiplexing; idle eviction; TUI `/conv` commands + spinners.
- **P4 — Migration + docs:** backfill from legacy; `--migrate-config` step; `--init` wizard; CHANGELOG; mdBook user docs.
