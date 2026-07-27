---
aliases:
  - Agent Identity
  - Agent Data Isolation
  - Multi-Agent Database Isolation
tags:
  - sdd
  - spec
  - database
  - multi-tenant
  - isolation
created: 2026-03-28
status: approved
related:
  - "[[MOC-specs]]"
  - "[[031-database-abstraction/spec]]"
  - "[[038-vault/spec]]"
---

# Agent Identity in the Shared Data Model

> **Scope**: Multi-agent database isolation for shared PostgreSQL deployments (cross-cutting)

## 1. Problem

With SQLite, exactly one Zeph agent process accesses the database (single-writer
guarantee, single user). With PostgreSQL, multiple agent instances can connect to
the same shared database simultaneously. Rows must be attributable to a specific
agent instance so that:

1. Each agent manages its own conversation history, memory, and state independently.
2. Agents can be isolated (agent A cannot read agent B's private memory).
3. Agents can share subsystems selectively (shared knowledge graph, shared code
   index, private conversations).
4. Migrations remain safe under concurrent startup.

## 2. Agent Identity Concept

An **agent identity** is a stable, human-readable string that uniquely identifies
a logical Zeph agent within a shared database. It is distinct from a runtime
process ID.

### Definition

| Concept | Type | Source | Purpose |
|---------|------|--------|---------|
| `agent_id` | `Arc<str>` (max 64 chars, `[a-z0-9_-]`) | Config field `[agent] id`, or hostname fallback | Primary isolation key in all DB queries |
| `instance_uuid` | `Uuid` (v7, time-ordered) | Generated at startup, never persisted in config | Fine-grained instance tracking in logs and metrics; NOT used for DB isolation |

### Resolution Order

At bootstrap, before pool construction:

1. If `[agent] id` is set in TOML config, use it verbatim.
2. Else, derive from system hostname: `hostname | tr 'A-Z' 'a-z' | tr -c 'a-z0-9_-' '-' | head -c 64`.
   **Amendment [2026-03-28]**: Note that dots in hostnames (e.g., `host.example.com`)
   are replaced with `-` (e.g., `host-example-com`). If the sanitized result starts
   with `-` (e.g., hostname `.local` becomes `-local`), fall back to `"default"`.
3. Validate against regex `^[a-z0-9][a-z0-9_-]{0,63}$`. Reject and fail startup if invalid.

### Relationship to `conversation_id`

`conversation_id` identifies a single conversation _within_ an agent. The hierarchy is:

```
agent_id  (logical agent — "my-bot", "team-shared", "default")
  └── conversation_id  (one of many conversations owned by that agent)
        └── message_id  (message within the conversation)
```

For single-agent SQLite deployments, `agent_id` defaults to `"default"` and is
invisible to the user.

### AgentId Newtype Wrapper

```rust
// zeph-db/src/identity.rs

/// Validated agent identifier. Immutable after construction.
///
/// Format: 1-64 characters, `[a-z0-9][a-z0-9_-]*`.
/// Used as the primary isolation key in all database queries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentId(Arc<str>);

impl AgentId {
    /// The default agent ID for single-agent deployments.
    pub const DEFAULT: &str = "default";

    /// Parse and validate an agent ID string.
    ///
    /// # Errors
    ///
    /// Returns an error if the string is empty, exceeds 64 characters,
    /// or contains characters outside `[a-z0-9_-]`.
    pub fn parse(s: impl Into<String>) -> Result<Self, AgentIdError> {
        let s = s.into();
        if s.is_empty() || s.len() > 64 {
            return Err(AgentIdError::InvalidLength(s.len()));
        }
        let bytes = s.as_bytes();
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return Err(AgentIdError::InvalidStart(s));
        }
        if !s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') {
            return Err(AgentIdError::InvalidCharacters(s));
        }
        Ok(Self(Arc::from(s)))
    }

    /// Return the default agent ID. Always valid.
    #[must_use]
    pub fn default_id() -> Self {
        Self(Arc::from(Self::DEFAULT))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentIdError {
    #[error("agent_id length must be 1-64 characters, got {0}")]
    InvalidLength(usize),
    #[error("agent_id must start with [a-z0-9], got: {0}")]
    InvalidStart(String),
    #[error("agent_id contains invalid characters (only [a-z0-9_-] allowed): {0}")]
    InvalidCharacters(String),
}
```

## 3. Isolation Model

Two modes govern how queries are scoped:

| Mode | Behavior | Use Case |
|------|----------|----------|
| **Isolated** (default) | Every query includes `WHERE agent_id = ?`. Agent A cannot see agent B's rows. | Conversations, episodic memory, scheduler jobs, session history |
| **Shared** (opt-in per subsystem) | Rows have `agent_id = NULL` (global) or are visible to all agents. Queries omit or ignore the `agent_id` filter. | Knowledge graph, code index metadata, MCP trust scores, plan cache |

### Per-Table Default Isolation Mode

| Table(s) | Default Mode | Rationale |
|-----------|-------------|-----------|
| `conversations`, `messages`, `summaries`, `mem_scenes`, `mem_scene_members` | **Isolated** | Conversation history is private per agent — this is the core isolation boundary |
| `embeddings_metadata`, `vector_points`, `vector_collections` | **Isolated** | Embeddings reference agent-specific messages |
| `input_history` | **Isolated** | User input is per-agent |
| `tool_overflow` | **Isolated** | Tool output belongs to a specific agent session |
| `session_digest` | **Isolated** | Per-conversation, per-agent |
| `user_corrections` | **Isolated** | User feedback is agent-specific |
| `learned_preferences` | **Isolated** | Learned from agent-specific interactions |
| `acp_sessions`, `acp_session_events` | **Isolated** | ACP sessions are per-agent |
| `experiment_results` | **Isolated** | Experiments are per-agent runs |
| `scheduled_jobs` | **Isolated** | Each agent manages its own schedule |
| `graph_entities`, `graph_edges`, `graph_communities`, `graph_entity_aliases`, `graph_metadata` | **Isolated** | **Amendment [2026-03-28]**: Changed from Shared to Isolated. In shared mode, graph entities extracted from private conversations leak derived information (entity names, edge types, timestamps) to other agents. Shared graph mode should only be used when all agents belong to the same trust domain. A `source_agent_id TEXT` nullable column is added for provenance tracking (distinct from the isolation `agent_id`). |
| `chunk_metadata` (code index) | **Shared** | Code index is read-only, no personal data, same codebase for all agents |
| `skill_usage`, `skill_versions`, `skill_outcomes`, `skill_trust` | **Shared** | Skills are shared infrastructure — trust scores and usage stats benefit all agents |
| `response_cache`, `semantic_response_cache` | **Shared** | Cache hits benefit all agents; duplicate caching wastes space |
| `compression_guidelines`, `compression_failure_pairs` | **Shared** | Learned compression heuristics apply globally |
| `mcp_trust_scores` | **Shared** | Trust in MCP servers is agent-independent |
| `plan_cache` | **Shared** | Cached plans are reusable across agents |
| `task_graphs` | **Isolated** | Task execution belongs to a specific agent session |

### Configurable Isolation Overrides

The mode for "Shared" tables can be switched to "Isolated" via config when strict
multi-tenancy is required (e.g., different customers sharing a database):

```toml
[database]
isolation = "isolated"                              # default
# Override defaults: make plan_cache and code_index agent-private
shared_subsystems = ["code_index", "response_cache", "skills", "mcp_trust", "compression"]
# Omitting "plan_cache" from this list makes it isolated
```

When `isolation = "shared"`, the listed subsystems use `agent_id IS NULL` (global)
for queries. When `isolation = "isolated"`, every subsystem is scoped to the agent
regardless of `shared_subsystems`.

### Shared-to-Isolated Mode Transition Requirements

**Amendment [2026-03-28]**: Switching a subsystem from Shared to Isolated mode creates
a data visibility gap: rows written in Shared mode have `agent_id = NULL`, but Isolated
mode filters with `WHERE agent_id = ?`, which never matches `NULL` (SQL three-valued logic).

Requirements:

1. **Data migration**: Before switching from Shared to Isolated, run:
   ```sql
   UPDATE <table> SET agent_id = 'my-agent' WHERE agent_id IS NULL;
   ```
   This must be documented in the config migration guide and the `--migrate-config`
   output when `shared_subsystems` changes.

2. **Startup check**: At bootstrap, after resolving `AgentScope` per subsystem,
   check if any newly-Isolated subsystem's tables contain `agent_id IS NULL` rows.
   If so, emit a warning:
   ```
   WARN: table 'graph_entities' has rows with agent_id = NULL but subsystem
   is configured as Isolated. These rows are invisible. Run:
     UPDATE graph_entities SET agent_id = '<agent_id>' WHERE agent_id IS NULL;
   ```

3. **Transitional query mode** (optional): During migration, Isolated-mode reads
   can use `WHERE (agent_id = ? OR agent_id IS NULL)` to include legacy NULL rows.
   This is opt-in via `[database] include_shared_rows = true` (default false) and
   should be disabled after migration is complete.

## 4. Schema Changes

### Column Addition Strategy

Every table gains an `agent_id` column. The column semantics depend on isolation mode:

- **Isolated tables**: `agent_id TEXT NOT NULL` — every row belongs to exactly one agent.
- **Shared tables**: `agent_id TEXT` (nullable) — `NULL` means the row is global/shared.

The migration (numbered `050_agent_identity.sql`) is part of the DB abstraction plan.
It runs during the same release that introduces `zeph-db`.

### SQLite Migration

```sql
-- Agent identity for multi-agent deployments.
--
-- For SQLite (single-agent), all existing rows get agent_id = 'default'.
-- The DEFAULT clause ensures new rows also get 'default' without code changes.
-- ALTER TABLE ADD COLUMN with constant DEFAULT is O(1) in SQLite (no table rewrite).

-- Isolated tables: NOT NULL with default 'default'
ALTER TABLE conversations         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE messages              ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE summaries             ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE input_history         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE tool_overflow         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE session_digest        ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE user_corrections      ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE learned_preferences   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE acp_sessions          ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE experiment_results    ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE task_graphs           ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE mem_scenes            ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE embeddings_metadata   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';

-- Shared tables: nullable, NULL = global
ALTER TABLE graph_entities        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_communities     ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE chunk_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_usage           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_versions        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_outcomes        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_trust           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE response_cache        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE compression_guidelines ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE plan_cache            ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_collections    ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_points         ADD COLUMN agent_id TEXT DEFAULT NULL;

-- Covering indexes for agent-scoped queries on high-traffic isolated tables.
-- Composite indexes with agent_id as prefix for efficient range scans.
CREATE INDEX IF NOT EXISTS idx_messages_agent_conv
    ON messages(agent_id, conversation_id, id)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_conversations_agent
    ON conversations(agent_id, id);

CREATE INDEX IF NOT EXISTS idx_summaries_agent_conv
    ON summaries(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_session_digest_agent
    ON session_digest(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_task_graphs_agent
    ON task_graphs(agent_id, status);

CREATE INDEX IF NOT EXISTS idx_mem_scenes_agent
    ON mem_scenes(agent_id);

CREATE INDEX IF NOT EXISTS idx_acp_sessions_agent
    ON acp_sessions(agent_id);

CREATE INDEX IF NOT EXISTS idx_experiment_results_agent
    ON experiment_results(agent_id, session_id);

-- **Amendment [2026-03-28]**: Additional composite indexes for tables identified
-- as missing coverage in the performance review.

-- embeddings_metadata: queried by (agent_id, conversation_id) in EmbeddingStore.
CREATE INDEX IF NOT EXISTS idx_embeddings_metadata_agent_conv
    ON embeddings_metadata(agent_id, conversation_id);

-- response_cache: queried by (agent_id, cache_key) when isolation is overridden.
CREATE INDEX IF NOT EXISTS idx_response_cache_agent_key
    ON response_cache(agent_id, cache_key);

-- **Amendment [2026-03-28]**: source_agent_id for graph provenance tracking.
-- Distinct from agent_id (which controls isolation). Records which agent
-- originally created the entity/edge, even in shared mode.
ALTER TABLE graph_entities ADD COLUMN source_agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges    ADD COLUMN source_agent_id TEXT DEFAULT NULL;
```

### PostgreSQL Migration

```sql
-- Agent identity for multi-agent deployments (PostgreSQL variant).

-- Isolated tables
ALTER TABLE conversations         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE messages              ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE summaries             ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE input_history         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE tool_overflow         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE session_digest        ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE user_corrections      ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE learned_preferences   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE acp_sessions          ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE experiment_results    ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE task_graphs           ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE mem_scenes            ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE embeddings_metadata   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';

-- Shared tables
ALTER TABLE graph_entities        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_communities     ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE chunk_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_usage           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_versions        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_outcomes        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_trust           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE response_cache        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE compression_guidelines ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE plan_cache            ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_collections    ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_points         ADD COLUMN agent_id TEXT DEFAULT NULL;

-- **Amendment [2026-03-28]**: All indexes use regular CREATE INDEX (not CONCURRENTLY).
-- CREATE INDEX CONCURRENTLY cannot run inside a transaction block, and sqlx::migrate!
-- runs each migration inside a transaction. Regular CREATE INDEX takes a brief
-- ACCESS EXCLUSIVE lock but is acceptable for a one-time migration.
-- For very large tables in production, concurrent index creation can be done
-- manually out-of-band after the migration.

CREATE INDEX IF NOT EXISTS idx_messages_agent_conv
    ON messages(agent_id, conversation_id, id)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_conversations_agent
    ON conversations(agent_id, id);

CREATE INDEX IF NOT EXISTS idx_summaries_agent_conv
    ON summaries(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_session_digest_agent
    ON session_digest(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_task_graphs_agent
    ON task_graphs(agent_id, status);

CREATE INDEX IF NOT EXISTS idx_mem_scenes_agent
    ON mem_scenes(agent_id);

CREATE INDEX IF NOT EXISTS idx_acp_sessions_agent
    ON acp_sessions(agent_id);

CREATE INDEX IF NOT EXISTS idx_experiment_results_agent
    ON experiment_results(agent_id, session_id);

-- **Amendment [2026-03-28]**: Additional composite indexes (see perf review F5, F6).
CREATE INDEX IF NOT EXISTS idx_embeddings_metadata_agent_conv
    ON embeddings_metadata(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_response_cache_agent_key
    ON response_cache(agent_id, cache_key);

-- **Amendment [2026-03-28]**: source_agent_id for graph provenance tracking.
ALTER TABLE graph_entities ADD COLUMN source_agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges    ADD COLUMN source_agent_id TEXT DEFAULT NULL;
```

### Primary Key Considerations

`agent_id` is **not** added to existing primary keys. Existing PKs (`id INTEGER
PRIMARY KEY AUTOINCREMENT` for most tables) remain the physical row identifier.
`agent_id` is enforced via:

1. Composite indexes for query performance (see above).
2. Application-level enforcement via `AgentScope` (see section 5).
3. For tables with natural UNIQUE constraints that should be per-agent (e.g.,
   `skill_usage.skill_name`, `scheduled_jobs.name`), add a new UNIQUE constraint
   on `(agent_id, skill_name)` and drop the old one — but only when the subsystem
   is in "isolated" mode. When shared, the existing UNIQUE constraint on the
   natural key remains correct.

Tables needing UNIQUE constraint updates in isolated mode:

| Table | Current UNIQUE | New UNIQUE (isolated) | Shared mode |
|-------|---------------|----------------------|-------------|
| `skill_usage` | `(skill_name)` | `(agent_id, skill_name)` | Keep `(skill_name)` |
| `scheduled_jobs`* | `(name)` | `(agent_id, name)` | N/A (always isolated) |
| `graph_entities` | `(name, entity_type)` | Keep (shared by default) | Keep |
| `response_cache` | `(cache_key)` | Keep (shared by default) | Keep |
| `plan_cache` | `(goal_hash)` | Keep (shared by default) | Keep |

\* `scheduled_jobs` is managed by `zeph-scheduler` with inline DDL; the UNIQUE
constraint update happens in the scheduler's migration path.

## 5. Query Layer: `AgentScope`

Rather than passing `agent_id` as a parameter to every store method (error-prone,
verbose), introduce an `AgentScope` wrapper that pre-binds the agent identity and
isolation mode. Every store receives an `AgentScope` at construction time.

```rust
// zeph-db/src/scope.rs

use crate::{AgentId, DbPool};
use std::sync::Arc;

/// Isolation mode for a database subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    /// Queries are scoped to a single agent_id. Other agents' rows are invisible.
    Isolated,
    /// Queries see all rows regardless of agent_id. Writes use agent_id = NULL.
    Shared,
}

/// Pre-bound database scope carrying pool + agent identity + isolation mode.
///
/// Constructed once at startup and cloned into each store. The `agent_id` and
/// `isolation` are immutable for the lifetime of the process.
#[derive(Debug, Clone)]
pub struct AgentScope {
    pool: DbPool,
    agent_id: AgentId,
    isolation: IsolationMode,
}

impl AgentScope {
    #[must_use]
    pub fn new(pool: DbPool, agent_id: AgentId, isolation: IsolationMode) -> Self {
        Self { pool, agent_id, isolation }
    }

    /// **Amendment [2026-03-28]**: `pool()` is `#[doc(hidden)]` with a
    /// deprecation note. Exposing the raw pool allows any store to bypass
    /// agent_id filtering by constructing a `GlobalScope`. Prefer using
    /// `AgentScope` query methods or passing `&AgentScope` to query helpers.
    #[doc(hidden)]
    #[deprecated(note = "direct pool access bypasses agent_id filtering; use AgentScope query methods")]
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    #[must_use]
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    #[must_use]
    pub fn isolation(&self) -> IsolationMode {
        self.isolation
    }

    /// Return the agent_id string to bind in isolated queries.
    ///
    /// Returns `Some(agent_id)` in Isolated mode, `None` in Shared mode.
    #[must_use]
    pub fn filter_value(&self) -> Option<&str> {
        match self.isolation {
            IsolationMode::Isolated => Some(self.agent_id.as_str()),
            IsolationMode::Shared => None,
        }
    }

    /// Return the agent_id to write on new rows.
    ///
    /// Isolated mode: the agent's ID string.
    /// Shared mode: None (NULL in the database).
    #[must_use]
    pub fn write_value(&self) -> Option<&str> {
        self.filter_value()
    }
}

/// Global scope for administrative operations (export, migration, cross-agent queries).
///
/// Bypasses agent_id filtering. Constructed explicitly by admin CLI commands,
/// never by the normal agent loop.
///
/// **Amendment [2026-03-28]**: `GlobalScope::new()` is `pub(crate)` and only
/// accessible from the root binary crate's admin/CLI path. This prevents
/// accidental construction in agent code. A `tracing::warn!` is emitted on
/// construction for audit purposes.
#[derive(Debug, Clone)]
pub struct GlobalScope {
    pool: DbPool,
}

impl GlobalScope {
    /// Construct a GlobalScope for admin operations.
    ///
    /// # Restriction
    ///
    /// This constructor is `pub(crate)` — only the root binary crate (or
    /// `zeph-db` internals) can create a `GlobalScope`. Agent code in
    /// consumer crates cannot construct this type.
    #[must_use]
    pub(crate) fn new(pool: DbPool) -> Self {
        tracing::warn!("GlobalScope constructed — bypasses all agent_id filtering");
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }
}
```

### Store Construction Changes

```rust
// Before (current):
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    pub async fn new(path: &str) -> Result<Self, MemoryError> { ... }
}

// After:
pub struct DbStore {
    scope: AgentScope,
}

impl DbStore {
    pub fn new(scope: AgentScope) -> Self {
        Self { scope }
    }
}
```

### Query Pattern — Isolated Table

```rust
// Before:
pub async fn load_history(
    &self,
    conversation_id: ConversationId,
    limit: i64,
) -> Result<Vec<MessageRow>, MemoryError> {
    let rows = sqlx::query_as(
        "SELECT ... FROM messages WHERE conversation_id = ? AND deleted_at IS NULL \
         ORDER BY id DESC LIMIT ?"
    )
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows)
}

// After:
pub async fn load_history(
    &self,
    conversation_id: ConversationId,
    limit: i64,
) -> Result<Vec<MessageRow>, MemoryError> {
    let rows = sqlx::query_as(sql!(
        "SELECT ... FROM messages \
         WHERE conversation_id = ? AND agent_id = ? AND deleted_at IS NULL \
         ORDER BY id DESC LIMIT ?"
    ))
    .bind(conversation_id)
    .bind(self.scope.agent_id().as_str())
    .bind(limit)
    .fetch_all(self.scope.pool())
    .await?;
    Ok(rows)
}
```

### Query Pattern — Shared Table (Knowledge Graph)

```rust
impl GraphStore {
    pub async fn find_entity(&self, name: &str) -> Result<Option<GraphEntity>, MemoryError> {
        // Shared mode: no agent_id filter.
        // Isolated mode (overridden): filter by agent_id.
        let query = match self.scope.filter_value() {
            Some(aid) => {
                sqlx::query_as(sql!(
                    "SELECT * FROM graph_entities WHERE name = ? AND agent_id = ?"
                ))
                .bind(name)
                .bind(aid)
                .fetch_optional(self.scope.pool())
                .await?
            }
            None => {
                sqlx::query_as(sql!(
                    "SELECT * FROM graph_entities WHERE name = ?"
                ))
                .bind(name)
                .fetch_optional(self.scope.pool())
                .await?
            }
        };
        Ok(query)
    }
}
```

### Simplifying with Helper

```rust
impl AgentScope {
    /// Append `AND agent_id = ?` to a query when in isolated mode.
    /// Returns the SQL suffix and an optional bind value.
    pub fn agent_filter_clause(&self) -> (&'static str, Option<&str>) {
        match self.isolation {
            IsolationMode::Isolated => (" AND agent_id = ?", Some(self.agent_id.as_str())),
            IsolationMode::Shared => ("", None),
        }
    }
}

pub async fn find_entity(&self, name: &str) -> Result<Option<GraphEntity>, MemoryError> {
    let (filter, bind_val) = self.scope.agent_filter_clause();
    let sql = format!("SELECT * FROM graph_entities WHERE name = ?{filter}");
    let mut q = sqlx::query_as(&sql!(&sql)).bind(name);
    if let Some(aid) = bind_val {
        q = q.bind(aid);
    }
    Ok(q.fetch_optional(self.scope.pool()).await?)
}
```

**Note**: The `format!` + conditional bind approach introduces a minor runtime
cost (string allocation) but keeps the code DRY. For hot-path queries, use
`LazyLock` with pre-built variants for both modes.

## 6. Configuration Design

### Config Type

Extend `AgentConfig` in `zeph-config/src/agent.rs`:

```rust
fn default_agent_id() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.to_str().map(str::to_owned))
        .map(|h| {
            h.to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .take(64)
                .collect()
        })
        .unwrap_or_else(|| "default".to_string())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AgentConfig {
    pub name: String,
    /// Stable agent identifier used as the DB isolation key in multi-agent deployments.
    /// Defaults to the system hostname (lowercased, sanitized).
    /// For single-agent SQLite: "default" is used implicitly.
    #[serde(default = "default_agent_id")]
    pub id: String,
    // ... existing fields unchanged ...
}
```

### TOML Surface

```toml
[agent]
name = "Zeph"
id = "my-agent"           # stable identifier, used as agent_id in DB

[database]
# Isolation mode: "isolated" (default) | "shared"
# "isolated": every subsystem is scoped to agent.id
# "shared": subsystems listed in shared_subsystems see global rows
isolation = "isolated"
# Subsystems that operate in shared mode when isolation = "shared".
# Ignored when isolation = "isolated".
# Valid values: "graph", "code_index", "skills", "response_cache",
#               "mcp_trust", "compression", "plan_cache"
shared_subsystems = ["graph", "code_index", "skills", "response_cache",
                     "mcp_trust", "compression", "plan_cache"]
```

### Config Enums

```rust
// zeph-config/src/memory.rs (additions)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IsolationMode {
    #[default]
    Isolated,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedSubsystem {
    Graph,
    CodeIndex,
    Skills,
    ResponseCache,
    McpTrust,
    Compression,
    PlanCache,
}
```

### Scope Construction at Startup

```rust
// Pseudocode — zeph-core/src/bootstrap.rs

let agent_id = AgentId::parse(&config.agent.id)?;
let pool = DbConfig::from(&config.memory).connect().await?;

// Determine isolation for each subsystem
let is_shared_mode = config.database.isolation == IsolationMode::Shared;

let make_scope = |subsystem: SharedSubsystem| -> AgentScope {
    let isolation = if is_shared_mode
        && config.database.shared_subsystems.contains(&subsystem)
    {
        zeph_db::IsolationMode::Shared
    } else {
        zeph_db::IsolationMode::Isolated
    };
    AgentScope::new(pool.clone(), agent_id.clone(), isolation)
};

// Conversations, messages — always isolated
let memory_scope = AgentScope::new(pool.clone(), agent_id.clone(), zeph_db::IsolationMode::Isolated);
// Knowledge graph — configurable
let graph_scope = make_scope(SharedSubsystem::Graph);
// Scheduler — always isolated
let scheduler_scope = AgentScope::new(pool.clone(), agent_id.clone(), zeph_db::IsolationMode::Isolated);
```

## 7. Concurrent Migration Safety

### SQLite

No concern. SQLite's single-writer lock serializes everything. Only one process
can write at a time, and the `busy_timeout` PRAGMA handles contention.

### PostgreSQL: `sqlx::migrate!` and Advisory Locks

`sqlx::migrate!().run(pool)` on PostgreSQL **already uses advisory locks**
internally. Specifically, sqlx acquires `pg_advisory_lock(hash)` before checking
the `_sqlx_migrations` table and running pending migrations. This means:

- Multiple Zeph instances starting simultaneously against the same PostgreSQL
  database will serialize their migration runs automatically.
- The first instance to acquire the lock runs all pending migrations.
- Subsequent instances wait for the lock, then find all migrations already applied,
  and proceed without running anything.

**No additional locking mechanism is needed.**

To confirm, the relevant sqlx source (as of 0.8.x):

```rust
// sqlx-core/src/migrate/migrate.rs (simplified)
async fn run(&self, pool: &PgPool) -> Result<()> {
    let lock_id = ... ; // hash of migration source path
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock_id)
        .execute(pool)
        .await?;
    // ... run pending migrations ...
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_id)
        .execute(pool)
        .await?;
}
```

**Risk**: If a process crashes while holding the advisory lock (between `lock`
and `unlock`), the lock is released automatically when the PostgreSQL session
ends (advisory locks are session-scoped). No manual cleanup is needed.

## 8. Impact on Existing SQLite Deployment

| Aspect | Impact |
|--------|--------|
| **Existing rows** | `agent_id` column added with `DEFAULT 'default'`. All existing rows get `agent_id = 'default'` at O(1) cost (SQLite stores the default in the schema, not per-row). |
| **New rows** | If `[agent] id` is not set in config, `agent_id` resolves to `"default"` (or hostname). The `DEFAULT 'default'` clause in DDL serves as a safety net for raw SQL inserts. |
| **Query performance** | The new `WHERE agent_id = 'default'` clause adds a constant-time comparison. The composite indexes ensure no scan regression. |
| **User action required** | None. Existing config files without `[agent] id` or `[database] isolation` work unchanged. |
| **Behavioral change** | Zero. Single-agent SQLite with `agent_id = 'default'` behaves identically to the current agent-unaware queries. |
| **Config migration** | `--migrate-config` adds `id = "default"` under `[agent]` and `isolation = "isolated"` under `[database]` if absent. Non-breaking. |
| **Database file size** | Negligible increase. `agent_id TEXT` column with constant `'default'` value is stored once in the schema header, not per-row (SQLite optimization for constant-default columns added via `ALTER TABLE ADD COLUMN`). Indexes add ~10-20% overhead on indexed tables. |

## 9. Risks and Mitigations

### Query Verbosity

**Risk**: Every `WHERE` clause gains `AND agent_id = ?`, increasing query
complexity and maintenance burden.

**Mitigation**: The `AgentScope::agent_filter_clause()` helper centralizes the
pattern. For isolated-only tables (conversations, messages), `agent_id = ?` is
always present — no conditional logic needed. The `sql!` macro already handles
placeholder rewriting, so `agent_id` is just another bind parameter.

### Index Coverage

**Risk**: Queries that previously used single-column indexes now need composite
indexes with `agent_id` prefix. Missing indexes cause sequential scans.

**Mitigation**: The migration (section 4) creates composite indexes for all
high-traffic query patterns. The `agent_id` prefix is chosen because it has low
cardinality (few distinct values) and PostgreSQL's query planner handles it well
with `Index Only Scan` on `(agent_id, conversation_id, id)`.

**Trade-off**: Index overhead is paid on every write (INSERT, UPDATE, DELETE).
For SQLite single-agent deployments, the composite indexes are redundant with the
existing single-column indexes. Acceptable overhead (~10-20% on indexed tables).

### Forgotten agent_id Filter (Data Leakage)

**Risk**: A query that omits the `agent_id` filter returns rows from all agents,
leaking private data across tenants.

**Mitigations (defense in depth)**:

1. **Type-system enforcement**: Stores receive `AgentScope`, not raw `DbPool`.
   The `AgentScope` API makes agent-scoped queries the path of least resistance.
   Accessing the raw pool requires `.pool()`, which is an explicit opt-out.

2. **Code review convention**: All new queries must go through `AgentScope`.
   Direct pool access is reserved for `GlobalScope` (admin operations).

3. **Integration test**: Add a test that introspects all SQL query strings in the
   codebase (via a build script or grep) and asserts that every query touching an
   isolated table contains `agent_id`.

### Cross-Agent Operations

**Risk**: Admin tools (data export, migration, global cleanup, analytics) need
to query across all agents.

**Mitigation**: `GlobalScope` type (see section 5) provides unfiltered pool access.
It is constructed explicitly in admin CLI commands (`zeph db export --all-agents`,
`zeph db stats`), never in the agent loop. The type distinction (`GlobalScope` vs
`AgentScope`) prevents accidental global queries in agent code.

### Agent ID Collisions

**Risk**: Two users independently choose the same `agent_id` (e.g., both use
hostname-derived IDs on identically-named hosts) and collide in a shared database.

**Mitigation**: Document that `agent_id` must be unique per logical agent in a
shared database. The `--init` wizard prompts for a unique ID. For automated
deployments, generate IDs from a namespace (e.g., `team-${KUBERNETES_POD_NAME}`).
No runtime enforcement of uniqueness — this is an operational concern, not a
database constraint.

### Migration Ordering

**Risk**: Agent identity migration depends on tables created by the original 49 migrations.
If the DB abstraction (moving migrations to `zeph-db`) and subsequent
(PostgreSQL + agent identity) are separate releases, the migration numbering must
be coordinated.

**Mitigation**: Agent identity migration (050) ships after all 49 migrations are
successfully ported to both backends. The migration number is reserved initially
(an empty `050_reserved_agent_identity.sql` placeholder) to prevent number conflicts.

## 10. Key Invariants

1. **`agent_id` is immutable for the lifetime of a process.** Once resolved at
   startup, it never changes. Hot-reloading config does not alter `agent_id`.

2. **Isolated tables always have `agent_id NOT NULL`.** No row in an isolated
   table can have `agent_id = NULL`. The `NOT NULL DEFAULT 'default'` DDL
   constraint enforces this at the database level.

3. **Shared tables use `NULL` for global rows.** Agent-specific rows in shared
   tables (when a subsystem is overridden to isolated) use the agent's ID.
   Global rows use `NULL`.

4. **`AgentScope` is the sole gateway to the database in agent code.** No store
   in the agent loop may hold a raw `DbPool` reference. Only `GlobalScope` in
   admin commands may bypass agent filtering.

5. **SQLite `'default'` is transparent.** A single-agent SQLite deployment with
   `agent_id = 'default'` is indistinguishable from the pre-agent-identity schema
   in terms of query results and performance.

6. **The conversations → messages hierarchy respects agent_id transitively.**
   If `conversations.agent_id = 'X'`, all messages in that conversation also have
   `agent_id = 'X'`. Application code enforces this; no cross-agent foreign key
   constraint exists (SQLite does not support CHECK constraints referencing other
   tables).
