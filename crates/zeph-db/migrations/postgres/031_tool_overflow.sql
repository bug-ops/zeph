CREATE TABLE IF NOT EXISTS tool_overflow (
    id              TEXT PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    content         BYTEA NOT NULL,
    byte_size       INTEGER NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_tool_overflow_conversation ON tool_overflow(conversation_id);
CREATE INDEX idx_tool_overflow_created_at   ON tool_overflow(created_at);
