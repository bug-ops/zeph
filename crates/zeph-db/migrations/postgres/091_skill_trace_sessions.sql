-- AutoSkill A1 idempotency table (spec 056).
CREATE TABLE IF NOT EXISTS skill_trace_sessions (
    session_id          TEXT    NOT NULL PRIMARY KEY,
    processed_at        BIGINT  NOT NULL,
    candidates_proposed INTEGER NOT NULL DEFAULT 0,
    candidates_saved    INTEGER NOT NULL DEFAULT 0,
    candidates_merged   INTEGER NOT NULL DEFAULT 0
);
