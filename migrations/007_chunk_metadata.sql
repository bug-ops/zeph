CREATE TABLE IF NOT EXISTS chunk_metadata (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    qdrant_id    TEXT    NOT NULL UNIQUE,
    file_path    TEXT    NOT NULL,
    content_hash TEXT    NOT NULL,
    line_start   INTEGER NOT NULL,
    line_end     INTEGER NOT NULL,
    language     TEXT    NOT NULL,
    node_type    TEXT    NOT NULL,
    entity_name  TEXT,
    indexed_at   TEXT    NOT NULL DEFAULT (datetime('now')),

    UNIQUE(file_path, content_hash)
);

CREATE INDEX IF NOT EXISTS idx_chunk_file_path ON chunk_metadata(file_path);
CREATE INDEX IF NOT EXISTS idx_chunk_hash ON chunk_metadata(content_hash);
