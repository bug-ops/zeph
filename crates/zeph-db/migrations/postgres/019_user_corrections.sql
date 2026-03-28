CREATE TABLE IF NOT EXISTS user_corrections (
    id               BIGSERIAL PRIMARY KEY,
    session_id       BIGINT,
    original_output  TEXT NOT NULL,
    correction_text  TEXT NOT NULL,
    skill_name       TEXT,
    correction_kind  TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_corrections_skill ON user_corrections(skill_name);
CREATE INDEX IF NOT EXISTS idx_corrections_session ON user_corrections(session_id);
