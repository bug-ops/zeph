-- D2Skill: per-step correction hints extracted from failed tool traces.
CREATE TABLE IF NOT EXISTS skill_step_corrections (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_name  TEXT NOT NULL,
    step        INTEGER NOT NULL,
    hint        TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_step_corrections_skill ON skill_step_corrections(skill_name);
