-- D2Skill: step-level error correction hints stored per skill.
CREATE TABLE IF NOT EXISTS step_corrections (
    id              BIGSERIAL PRIMARY KEY,
    skill_name      TEXT NOT NULL,
    failure_kind    TEXT NOT NULL DEFAULT '',
    error_substring TEXT NOT NULL DEFAULT '',
    tool_name       TEXT NOT NULL DEFAULT '',
    hint            TEXT NOT NULL,
    use_count       BIGINT NOT NULL DEFAULT 0,
    success_count   BIGINT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(skill_name, failure_kind, error_substring, tool_name)
);

CREATE INDEX IF NOT EXISTS idx_step_corrections_skill ON step_corrections(skill_name, failure_kind);
