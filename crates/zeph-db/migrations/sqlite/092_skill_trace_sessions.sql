-- AutoSkill A1 idempotency table (spec 056).
-- Records each session that has been processed by the trace extraction pipeline
-- to prevent re-extraction after agent restart or config reload.
CREATE TABLE IF NOT EXISTS skill_trace_sessions (
    session_id          TEXT    NOT NULL PRIMARY KEY,
    processed_at        INTEGER NOT NULL,
    candidates_proposed INTEGER NOT NULL DEFAULT 0,
    candidates_saved    INTEGER NOT NULL DEFAULT 0,
    candidates_merged   INTEGER NOT NULL DEFAULT 0
);
