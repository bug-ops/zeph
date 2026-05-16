-- Trajectory-informed memory (#2498).
-- Stores per-conversation procedural and episodic entries extracted from tool-call turns.
CREATE TABLE IF NOT EXISTS trajectory_memory (
    id              BIGSERIAL PRIMARY KEY,
    conversation_id BIGINT REFERENCES conversations(id),
    turn_index      BIGINT NOT NULL,
    kind            TEXT NOT NULL CHECK(kind IN ('procedural', 'episodic')),
    intent          TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    tools_used      TEXT NOT NULL DEFAULT '[]',
    confidence      REAL NOT NULL DEFAULT 0.8,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_trajectory_kind ON trajectory_memory(kind);
CREATE INDEX IF NOT EXISTS idx_trajectory_conversation ON trajectory_memory(conversation_id);

-- Per-conversation extraction watermark: tracks the last message id processed per conversation.
CREATE TABLE IF NOT EXISTS trajectory_meta (
    conversation_id BIGINT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    last_extracted_message_id BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
