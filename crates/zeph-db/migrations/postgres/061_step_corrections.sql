-- D2Skill: per-step correction hints extracted from failed tool traces.
CREATE TABLE IF NOT EXISTS skill_step_corrections (
    id          BIGSERIAL PRIMARY KEY,
    skill_name  TEXT NOT NULL,
    step        INTEGER NOT NULL,
    hint        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_step_corrections_skill ON skill_step_corrections(skill_name);
