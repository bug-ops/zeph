-- Session digest: one distilled NL digest per conversation, updated at session end.
CREATE TABLE IF NOT EXISTS session_digest (
    id              BIGSERIAL PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    digest          TEXT NOT NULL,
    token_count     INTEGER NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_session_digest_conversation
    ON session_digest(conversation_id);
