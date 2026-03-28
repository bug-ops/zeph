CREATE TABLE IF NOT EXISTS embeddings_metadata (
    id             BIGSERIAL PRIMARY KEY,
    message_id     BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    qdrant_point_id TEXT NOT NULL,
    model          TEXT NOT NULL DEFAULT 'qwen3-embedding',
    dimensions     INTEGER NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(message_id, model)
);

CREATE INDEX IF NOT EXISTS idx_embeddings_metadata_message_id
    ON embeddings_metadata(message_id);
