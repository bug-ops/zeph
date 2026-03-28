-- Compression guidelines: learns from compaction failures to improve summarization quality.
-- Guidelines document (singleton, global scope).
CREATE TABLE IF NOT EXISTS compression_guidelines (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    version     INTEGER NOT NULL DEFAULT 1,
    guidelines  TEXT    NOT NULL DEFAULT '',
    token_count INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_compression_guidelines_version
    ON compression_guidelines(version DESC);

-- Failure pairs: compressed context snapshot + agent response showing context loss.
CREATE TABLE IF NOT EXISTS compression_failure_pairs (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id    INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    compressed_context TEXT    NOT NULL,
    failure_reason     TEXT    NOT NULL,
    created_at         TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    used_in_update     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_failure_pairs_used
    ON compression_failure_pairs(used_in_update, created_at);

CREATE INDEX IF NOT EXISTS idx_failure_pairs_conversation
    ON compression_failure_pairs(conversation_id);
