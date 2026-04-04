-- Advisory entity locking for multi-agent GraphStore coordination (#2478).
-- Prevents duplicate entity resolution when multiple sessions write concurrently.
-- Locks are soft: expired locks are reclaimed rather than blocking permanently.
CREATE TABLE IF NOT EXISTS entity_advisory_locks (
    entity_name TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    acquired_at TEXT NOT NULL DEFAULT (datetime('now')),
    -- TTL set to 120s to cover worst-case slow LLM calls
    expires_at TEXT NOT NULL DEFAULT (datetime('now', '+120 seconds'))
);
