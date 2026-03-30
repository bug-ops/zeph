-- ERL: experiential heuristics extracted from successful tasks.
-- MVP: no embedding column (exact skill_name match only; semantic retrieval is a future enhancement).
CREATE TABLE IF NOT EXISTS skill_heuristics (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    -- NULL means the heuristic applies to any skill (general).
    skill_name     TEXT,
    heuristic_text TEXT NOT NULL,
    confidence     REAL NOT NULL DEFAULT 0.5,
    use_count      INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Fast lookup by skill name; covers the common query pattern (skill_name, confidence DESC).
CREATE INDEX IF NOT EXISTS idx_skill_heuristics_name_conf ON skill_heuristics(skill_name, confidence DESC);
