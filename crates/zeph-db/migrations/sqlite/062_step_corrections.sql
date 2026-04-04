-- D2Skill: step-level error correction hints stored per skill.
-- Populated from ARISE failure traces; injected into context on tool failure.
CREATE TABLE IF NOT EXISTS step_corrections (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_name      TEXT NOT NULL,
    failure_kind    TEXT NOT NULL DEFAULT '',      -- FailureKind::as_str() or '' for any
    error_substring TEXT NOT NULL DEFAULT '',     -- substring match on error_context
    tool_name       TEXT NOT NULL DEFAULT '',     -- empty = any tool
    hint            TEXT NOT NULL,
    use_count       INTEGER NOT NULL DEFAULT 0,
    success_count   INTEGER NOT NULL DEFAULT 0,   -- times hint led to success on retry
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(skill_name, failure_kind, error_substring, tool_name) ON CONFLICT IGNORE
);

CREATE INDEX IF NOT EXISTS idx_step_corrections_skill ON step_corrections(skill_name, failure_kind);
