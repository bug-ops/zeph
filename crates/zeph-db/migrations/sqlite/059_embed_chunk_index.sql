-- Add chunk_index to embeddings_metadata to support multi-vector chunked embeddings.
-- SQLite does not support ALTER TABLE DROP CONSTRAINT, so we recreate the table.
CREATE TABLE embeddings_metadata_v2 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL DEFAULT 0,
    qdrant_point_id TEXT NOT NULL,
    model TEXT NOT NULL DEFAULT 'qwen3-embedding',
    dimensions INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(message_id, chunk_index, model)
);

INSERT INTO embeddings_metadata_v2
    (id, message_id, chunk_index, qdrant_point_id, model, dimensions, created_at)
SELECT id, message_id, 0, qdrant_point_id, model, dimensions, created_at
FROM embeddings_metadata;

DROP TABLE embeddings_metadata;
ALTER TABLE embeddings_metadata_v2 RENAME TO embeddings_metadata;

CREATE INDEX IF NOT EXISTS idx_embeddings_metadata_message_id
    ON embeddings_metadata(message_id);
