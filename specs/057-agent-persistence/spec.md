---
aliases:
  - Agent Persistence Service
  - Message Durability
  - Conversation History
tags:
  - sdd
  - spec
  - persistence
  - memory
  - durability
created: 2026-05-17
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[004-memory/spec]]"
  - "[[032-database-abstraction/spec]]"
  - "[[010-security/spec]]"
---

# Spec: Agent Persistence Service (`zeph-agent-persistence`)

> [!info]
> Stateless façade for loading conversation history from and persisting agent messages to
> the `SemanticMemory` backend (SQLite + Qdrant). Provides history sanitization, embedding
> decisions, tool-pair validation, and message metadata tracking without depending on `zeph-core`.

## 1. Overview

### Problem Statement

Agent message persistence — loading history, writing new messages, sanitizing orphaned tool pairs,
deciding on embeddings — is complex logic that touches both SQLite (for durability) and Qdrant
(for semantic retrieval). This logic was initially embedded in `zeph-core`, creating tight coupling
and making testing difficult. The dependency on `zeph-core` forced memory, context, and tool subsystems
to coordinate through a single shared type.

### Goal

Extract message persistence into a dedicated crate (`zeph-agent-persistence`) that:

1. **Decouples memory, context, and tool-dispatch logic** — persistence becomes a pure async service
   callable from tool dispatcher or agent loop without forming circular dependencies.
2. **Enables borrow-lens patterns** — `MemoryPersistenceView`, `SecurityView`, `MetricsView` allow
   the call site in `zeph-core` to prove disjoint borrows; the persistence service sees only what
   it needs, avoiding opaque mut bundles.
3. **Centralizes tool-pair sanitization** — orphaned `ToolUse`/`ToolResult` pairs are removed during
   history load and marked for soft-delete in SQLite (M3 defense against tail latency bugs).
4. **Provides clear contracts** — `LoadHistoryParams`, `LoadHistoryOutcome`, `PersistMessageRequest`,
   `PersistMessageOutcome` make every step of persistence observable and testable.

### Out of Scope

- Configuration of memory backends (owned by `zeph-config`)
- Semantic memory implementation (owned by `zeph-memory`)
- Tool execution and feedback detection (owned by `zeph-agent-tools`)
- LLM provider routing (owned by `zeph-llm`)
- Database driver abstraction (owned by `zeph-db`)

---

## 2. User Stories

### US-001: Load Conversation History

AS A agent loop starting or resuming a session
I WANT to restore the last N messages from SQLite with tool-pair sanitization
SO THAT the conversation context is complete and orphaned tool calls do not cause subsequent errors.

**Acceptance criteria:**

```
GIVEN a conversation ID and a SemanticMemory backend
WHEN PersistenceService::load_history() is called
THEN messages are fetched from SQLite (agent_visible=true)
AND orphaned ToolUse/ToolResult pairs are removed
AND removed message IDs are recorded for soft-delete
AND LoadHistoryOutcome reports counts and DB totals
```

### US-002: Persist User and Assistant Messages

AS A agent loop or tool dispatcher
I WANT to write each message to SQLite with optional Qdrant embedding
SO THAT conversation state is durable and retrievable for future sessions.

**Acceptance criteria:**

```
GIVEN a message to persist with role, content, and parts
WHEN PersistenceService::persist_message() is called
THEN the message is saved to SQLite with auto-generated message_id
AND embedding is performed if should_embed() returns true
AND exfiltration guard prevents embedding when injection flags are set
AND PersistMessageOutcome reports success, embedding status, and bytes written
```

### US-003: Sanitize Orphaned Tool Pairs

AS A conversation history loader
I WANT to detect and remove incomplete ToolUse/ToolResult sequences
SO THAT missing tool results do not cause parsing errors or hanging tool-use messages.

**Acceptance criteria:**

```
GIVEN a message buffer with potential orphans (trailing ToolUse, leading ToolResult, mid-history mismatches)
WHEN sanitize_tool_pairs() is called
THEN all four failure modes are handled
AND removed message IDs are returned for soft-delete in SQLite
AND logs record each orphan removal at WARN level
```

### US-004: Decide on Message Embedding

AS A persistence service
I WANT a decision function that evaluates whether a message should be embedded into Qdrant
SO THAT sensitive content can skip embedding and short messages do not add noise to memory.

**Acceptance criteria:**

```
GIVEN message role, content, parts, and security context
WHEN should_embed_message() is called
THEN embedding is skipped when:
  - skip_embedding flag is true (exfiltration guard)
  - parts contain [skipped] or [stopped] ToolResult markers
  - role is Assistant, autosave is disabled, or content is too short
AND all other messages embed by default
```

### US-005: Track Persistence Metrics

AS A monitoring system
I WANT per-message tracking of embedding status, bytes written, and security events
SO THAT I can audit message flow and detect anomalies.

**Acceptance criteria:**

```
GIVEN a persisted message with outcome details
WHEN metrics are updated
THEN sqlite_message_count, embeddings_generated, and exfiltration_memory_guards are incremented
AND values are available to the metrics subsystem for Prometheus export
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `LoadHistoryParams` is populated with memory, conversation ID, and mutable buffers THEN `load_history()` SHALL fetch up to 50 agent-visible messages (`agent_visible=true`) from SQLite | must |
| FR-002 | WHEN loaded messages include empty or orphaned messages THEN they SHALL be skipped and counted in the outcome | must |
| FR-003 | WHEN `sanitize_tool_pairs()` receives a message buffer THEN it SHALL remove: (1) trailing assistant ToolUse without matching ToolResult, (2) leading user ToolResult without preceding ToolUse, (3) mid-history orphaned ToolUse/ToolResult, and (4) unmatched ToolResult parts | must |
| FR-004 | WHEN orphaned messages are removed THEN their SQLite `db_id` values SHALL be collected and returned for soft-delete | must |
| FR-005 | WHEN `persist_message()` is called with memory disabled (None) THEN the operation SHALL return early with zero-filled outcome | must |
| FR-006 | WHEN message parts cannot be serialized to JSON THEN persistence SHALL fail gracefully, log an error, and return `None` to avoid creating orphaned tool-pair records | must |
| FR-007 | WHEN `exfiltration_guard` is active AND `has_injection_flags` is true AND `security.guard_memory_writes` is true THEN Qdrant embedding SHALL be skipped | must |
| FR-008 | WHEN `should_embed_message()` evaluates a message THEN it SHALL respect: skip_embedding flag, [skipped]/[stopped] ToolResult markers, assistant autosave setting, and content length threshold | must |
| FR-009 | WHEN message embedding is deferred to background THEN `memory.reap_embed_tasks()` SHALL be called to allow background task batching | must |
| FR-010 | WHEN `last_persisted_message_id` is set THEN subsequent history loads SHALL fetch only newer messages (LIMIT-based pagination) | must |
| FR-011 | WHEN a `PersistenceService` method encounters a database error THEN it SHALL log at ERROR level and return an error or `None`, never panic | must |
| FR-012 | WHEN `serialize_parts_json()` is called THEN it SHALL serde-serialize parts to a flat JSON array; empty parts returns `"[]"` | must |
| FR-013 | WHEN `has_meaningful_content()` evaluates a message THEN it SHALL identify legacy tool bracket markers and strip them; content with only markers is considered empty | must |
| FR-014 | WHEN metrics are updated via `MetricsView` THEN `sqlite_message_count`, `embeddings_generated`, and `exfiltration_memory_guards` counters SHALL be incremented atomically | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Modularity | `zeph-agent-persistence` SHALL NOT depend on `zeph-core`; it is callable only through borrow-lens views constructed at the call site |
| NFR-002 | Borrow checking | All three `*View` types use lifetime parameters to bind mutable references; the borrow checker at the call site can prove disjoint field borrows |
| NFR-003 | Async runtime | All I/O operations are async; the crate depends on tokio but does not establish the runtime itself |
| NFR-004 | Error handling | `PersistenceError` distinguishes between database errors and validation failures; all errors are loggable |
| NFR-005 | Safety | No `unsafe` code |
| NFR-006 | Observability | Key operations (load_history, persist_message, tool-pair sanitization) emit tracing spans with context (conversation ID, role, message count) |
| NFR-007 | Minimal abstraction | The service is stateless (`&self` methods) and has no internal state; it is a pure interface to `SemanticMemory` and LLM providers |

---

## 5. Data Model

### Core Structures

| Entity | Module | Description |
|--------|--------|-------------|
| `PersistenceService` | `service.rs` | Stateless façade with async methods for history load and message persistence |
| `LoadHistoryParams` | `service.rs` | Input bundle: message buffer, conversation ID, metadata buffers, memory view |
| `LoadHistoryOutcome` | `request.rs` | Output: count of loaded/skipped messages, orphan removal count, total DB counts |
| `PersistMessageRequest` | `request.rs` | Fully-owned input: role, content, parts, injection flags |
| `PersistMessageOutcome` | `request.rs` | Output: message ID, embedding status, redaction flag, bytes written |
| `MemoryPersistenceView` | `state.rs` | Borrow-lens over memory, conversation ID, autosave settings, goal text, unsummarized count |
| `SecurityView` | `state.rs` | Borrow-lens over `guard_memory_writes` flag |
| `MetricsView` | `state.rs` | Mutable borrow-lens over metrics counters |
| `ProviderHandles` | `state.rs` | Arc-backed LLM and embedding provider handles for background tasks |

### History Load Pipeline

```
load_history(LoadHistoryParams)
  ↓
fetch_history_filtered(cid, limit=50, agent_visible=true)  [SQLite]
  ↓
skip empty messages
  ↓
sanitize_tool_pairs(&mut messages)  [remove orphans, collect db_ids]
  ↓
soft_delete_messages(db_ids)  [SQLite, best-effort]
  ↓
count sqlite_total, semantic_total
  ↓
update last_persisted_message_id
  ↓
return LoadHistoryOutcome
```

### Message Persist Pipeline

```
persist_message(PersistMessageRequest, ...)
  ↓
serialize_parts_json(parts, role)  [JSON serialization]
  ↓
evaluate: guard_active = (security.guard_memory_writes && has_injection_flags)
  ↓
should_embed_message(skip_embedding=guard_active, parts, role, ...)
  ↓
if should_embed:
    remember_with_parts(cid, role, content, parts_json, goal_text)  [A-MAC + Qdrant]
  else:
    save_only(cid, role, content, parts_json)  [SQLite only]
  ↓
update last_persisted_message_id, unsummarized_count
  ↓
update metrics (sqlite_message_count, embeddings_generated, exfiltration guards)
  ↓
reap_embed_tasks()  [allow background batching]
  ↓
return PersistMessageOutcome
```

### Tool-Pair Sanitization

Orphaned `ToolUse`/`ToolResult` messages are detected and removed in four phases:

1. **Trailing orphans**: Remove assistant messages at the end of the buffer with `ToolUse` parts
   but no following user message with matching `ToolResult` parts.
2. **Leading orphans**: Remove user messages at the start with `ToolResult` parts but no preceding
   assistant message with matching `ToolUse` parts.
3. **Mid-history orphaned ToolUse**: Strip `ToolUse` parts from assistant messages whose tool IDs
   are not matched in the immediately following user message. Remove the message if no content remains.
4. **Mid-history orphaned ToolResult**: Strip `ToolResult` parts from user messages whose IDs
   are not matched in the preceding assistant message. Remove if no content remains.

Each removal logs a WARN trace and records the message's `db_id` for soft-delete in SQLite.

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `memory` is `None` (memory disabled) | Return zero-filled outcome immediately; do not attempt DB access |
| `conversation_id` is `None` | Return zero-filled outcome; waiting for first message sets the ID |
| Empty message content and no parts | Skip the message and increment skip counter |
| JSON serialization of parts fails | Log error, return `None`, skip persistence to avoid orphaned tool pairs |
| SQLite soft-delete fails after orphan collection | Log WARN, continue; orphaned records will load again until manually cleaned |
| Qdrant embedding timeout or error | Log error; the message is still in SQLite (save_only mode as fallback) |
| Very large message (>100 KB) | Persist as-is (no size limit at service level; config controls embedding thresholds) |
| Malformed legacy tool bracket syntax in content | `has_meaningful_content()` treats malformed tags as meaningful text |
| Empty `last_persisted_message_id` at first call | Load from absolute history start (no OFFSET in query) |
| Concurrent loads on same conversation | Both queries execute independently; last_persisted_message_id may race (handled by app-level session lock) |

---

## 7. Integration Points

### Callers in `zeph-core`

`zeph-core` holds the agent state and constructs borrow-lens views:

```rust
let memory_view = MemoryPersistenceView {
    memory: self.memory.as_ref(),
    conversation_id: self.conversation_id,
    autosave_assistant: self.config.memory.autosave_assistant,
    autosave_min_length: self.config.memory.autosave_min_length,
    unsummarized_count: &mut self.unsummarized_count,
    goal_text: self.current_goal.clone(),
};

let result = self.persistence_svc.load_history(LoadHistoryParams { ... })?;
```

### Dependency on `zeph-memory`

- `SemanticMemory` — loads history, persists with embedding, soft-deletes messages
- `ConversationId` — opaque type wrapping conversation identifiers
- `MessageId` — opaque type for SQLite message IDs

### Dependency on `zeph-llm`

- `Message`, `MessagePart`, `Role` — message types from provider abstraction
- `MessageMetadata` — includes `db_id`, `embedding_stored` flags

### Dependency on `zeph-config`

- `Config` — for autosave thresholds and memory settings (read via `MemoryPersistenceView`)

---

## 8. Key Invariants

### Persistence Invariant

Every message persisted to the agent conversation is written to SQLite and optionally to Qdrant.
Once written, the message ID is recorded in `last_persisted_message_id` so subsequent history
loads fetch only newer messages (LIMIT-based pagination).

**Guarantee**: No message is loaded twice (except after manual compaction/recovery).

### Tool-Pair Completeness Invariant

For every `ToolUse` message part in a persisted assistant message, there MUST be a corresponding
`ToolResult` message part in the immediately following user message, or the pair is sanitized
at history-load time.

**Guarantee**: Restored history never has dangling tool calls that await results.

### Exfiltration Guard Invariant

When `security.guard_memory_writes` is active AND a message has `has_injection_flags=true`,
the message is persisted to SQLite but NOT embedded into Qdrant. This prevents injection
patterns from contaminating semantic memory.

**Guarantee**: Qdrant index does not reflect flagged injection content.

### Unsummarized Count Invariant

`unsummarized_count` is incremented once per message persisted via `persist_message()`.
It is reset to zero when compaction runs. This tracks the age of the message buffer for
compaction scheduling.

**Guarantee**: Compaction triggers when unsummarized messages exceed a threshold.

---

## 9. NEVER Constraints

- **NEVER** embed a message whose parts contain `[skipped]` or `[stopped]` ToolResult markers —
  these are internal policy markers carrying no semantic value.
- **NEVER** panic in `persist_message()` or `load_history()` — all errors must be loggable
  and non-fatal (conversation continues).
- **NEVER** double-persist a message — `last_persisted_message_id` monotonically increases;
  no history reloads past that point unless it is explicitly reset.
- **NEVER** create orphaned tool-pair records in SQLite — if parts cannot be serialized,
  skip the entire message rather than storing partial state.
- **NEVER** allow plugin or user code to bypass the borrow-lens views and directly construct
  `*View` types — borrow-lens construction is reserved for `zeph-core` only.
- **NEVER** mutate `MemoryPersistenceView::memory` or `MemoryPersistenceView::conversation_id`
  after creation — the service treats them as read-only reference snapshots.
- **NEVER** change the SQL limit (currently 50 messages per load) without updating tests
  and compaction triggers — history load size is part of the performance contract.

---

## 10. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Dependency isolation | `cargo check -p zeph-agent-persistence` does not depend on `zeph-core` |
| SC-002 | Tool-pair sanitization | Unit tests cover all four orphan removal modes; no orphaned pairs in integration tests |
| SC-003 | Embedding decisions | Unit tests verify that guard_active, autosave, and content length threshold all work correctly |
| SC-004 | Error handling | All database errors are caught and logged; no panics in happy or error paths |
| SC-005 | Observation | All key operations emit `tracing::info_span!` spans with conversation ID and message count |

---

## 11. Agent Boundaries

### Always (without asking)
- Emit tracing spans with context (cid, role, content length) for all I/O operations
- Log all orphan removals at WARN level with affected tool IDs
- Keep the service stateless (no internal state, all I/O flows through method params)

### Ask First
- Adding new `*View` types or changing lifetime signatures
- Changing the SQL LIMIT for history fetch (currently 50 messages)
- Adding new embedding decision logic or overriding `should_embed_message()`

### Never
- Import from `zeph-core`
- Use `unsafe` code
- Panic in response to invalid input — always return an error
- Directly mutate mutable borrows inside `MemoryPersistenceView` (borrow at call site only)

---

## 12. Open Questions

None.

---

## 13. See Also

- [[001-system-invariants/spec]] — system-wide invariants
- [[002-agent-loop/spec]] — agent loop and context pressure
- [[004-memory/spec]] — SemanticMemory backend
- [[010-security/spec]] — exfiltration guard and security model
- [[032-database-abstraction/spec]] — database abstraction layer for SQLite/PostgreSQL
