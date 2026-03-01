CREATE TABLE IF NOT EXISTS user_corrections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER,
    original_output TEXT NOT NULL,
    correction_text TEXT NOT NULL,
    skill_name TEXT,
    correction_kind TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_corrections_skill ON user_corrections(skill_name);
CREATE INDEX IF NOT EXISTS idx_corrections_session ON user_corrections(session_id);
