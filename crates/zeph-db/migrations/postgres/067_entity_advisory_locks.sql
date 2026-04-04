-- Advisory entity locking for multi-agent GraphStore coordination (#2478).
CREATE TABLE IF NOT EXISTS entity_advisory_locks (
    entity_name TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at  TIMESTAMPTZ NOT NULL DEFAULT (NOW() + INTERVAL '120 seconds')
);
