CREATE TABLE IF NOT EXISTS summaries (
    id                BIGSERIAL PRIMARY KEY,
    conversation_id   BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    content           TEXT NOT NULL,
    first_message_id  BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    last_message_id   BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    token_estimate    INTEGER NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_summaries_conversation
    ON summaries(conversation_id);
