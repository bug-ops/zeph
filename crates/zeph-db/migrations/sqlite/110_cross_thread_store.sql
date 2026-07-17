-- Cross-thread key-value store (spec-080, #6363): a generic namespaced KV primitive a
-- graph node (or, later, other subsystems) can use to read and write state that outlives
-- a single task and is visible to other tasks in the same graph/owner scope. LangGraph
-- `Store` parity — see specs/080-cross-thread-store-dynamic-handoff/spec.md §5.3.
--
-- `owner_key` is NOT NULL DEFAULT 'local' (never nullable) — a NULL composite PK column
-- is invalid on PostgreSQL and would silently break upsert semantics on SQLite (spec §6
-- Never). `namespace` is a hierarchical string convention (e.g. `orch/{graph_id}`);
-- prefix scans via `namespace LIKE ?||'%'` power `list`/`search`. `value` is a JSON
-- payload. `version` is bumped on every upsert and gates optimistic-concurrency writes
-- via `WHERE version = ?` + rows-affected check.
CREATE TABLE IF NOT EXISTS cross_thread_store (
    owner_key  TEXT NOT NULL DEFAULT 'local',
    namespace  TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    version    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (owner_key, namespace, key)
);

-- Serves owner+namespace-scoped prefix scans (`store_list`/`store_search`).
CREATE INDEX IF NOT EXISTS idx_cross_thread_store_owner_ns
    ON cross_thread_store(owner_key, namespace);
