-- Goal lifecycle: stores long-horizon goals with FSM-controlled status transitions.
-- At most one 'active' goal is allowed at any time (partial unique index).
CREATE TABLE IF NOT EXISTS zeph_goals (
    id              TEXT PRIMARY KEY,
    text            TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active','paused','completed','cleared')),
    token_budget    INTEGER,
    turns_used      INTEGER NOT NULL DEFAULT 0,
    tokens_used     INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    completed_at    TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_zeph_goals_single_active
    ON zeph_goals(status) WHERE status = 'active';
CREATE INDEX IF NOT EXISTS idx_zeph_goals_status_created
    ON zeph_goals(status, created_at);
