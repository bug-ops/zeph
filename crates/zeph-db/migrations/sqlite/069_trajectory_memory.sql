-- Trajectory-informed memory (#2498).
-- Stores per-conversation procedural and episodic entries extracted from tool-call turns.
CREATE TABLE IF NOT EXISTS trajectory_memory (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER REFERENCES conversations(id),
    turn_index      INTEGER NOT NULL,
    kind            TEXT NOT NULL CHECK(kind IN ('procedural', 'episodic')),
    intent          TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    tools_used      TEXT NOT NULL DEFAULT '[]',
    confidence      REAL NOT NULL DEFAULT 0.8,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_trajectory_kind ON trajectory_memory(kind);
CREATE INDEX IF NOT EXISTS idx_trajectory_conversation ON trajectory_memory(conversation_id);

-- Per-conversation extraction watermark: tracks the last message id processed per conversation.
-- Using conversation_id as PK to support concurrent conversations (critic S1 fix).
CREATE TABLE IF NOT EXISTS trajectory_meta (
    conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    last_extracted_message_id INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
