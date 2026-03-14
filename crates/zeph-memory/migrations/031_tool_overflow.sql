CREATE TABLE IF NOT EXISTS tool_overflow (
    id              TEXT    PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    content         BLOB    NOT NULL,
    byte_size       INTEGER NOT NULL,
    created_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_tool_overflow_conversation ON tool_overflow(conversation_id);
CREATE INDEX idx_tool_overflow_created_at   ON tool_overflow(created_at);
