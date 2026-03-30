-- ARISE/STEM: tool usage log for pattern detection.
-- Retention: rows older than stem_retention_days are pruned by the STEM subsystem.
CREATE TABLE IF NOT EXISTS skill_usage_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Compact JSON array of tool names (normalized: no spaces after separators).
    tool_sequence   TEXT NOT NULL,
    -- blake3 hex hash of the normalized tool_sequence (16 chars) for fast grouping.
    sequence_hash   TEXT NOT NULL,
    context_hash    TEXT NOT NULL,
    outcome         TEXT NOT NULL CHECK (outcome IN ('success', 'failure')),
    conversation_id INTEGER,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_skill_usage_log_seq_hash ON skill_usage_log(sequence_hash);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_created ON skill_usage_log(created_at);
