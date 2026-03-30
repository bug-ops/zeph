-- Migration 054: Memex archive type + ACON per-category guidelines
--
-- Part 1: Add archive_type column to tool_overflow so compaction-time archives
--         are excluded from the short-lived overflow cleanup job (fix C2).
--
-- archive_type values:
--   'overflow' — execution-time overflow (existing rows, short-lived)
--   'archive'  — compaction-time Memex archive (long-lived, excluded from cleanup)
--
ALTER TABLE tool_overflow ADD COLUMN archive_type TEXT NOT NULL DEFAULT 'overflow';

CREATE INDEX IF NOT EXISTS idx_tool_overflow_archive_type
    ON tool_overflow(archive_type);

-- Part 2: Add category column to compression_guidelines and compression_failure_pairs
--         so the ACON updater can maintain per-content-type guidelines.
--
-- category values for compression_guidelines:
--   'global'              — global guideline, current behavior (default)
--   'tool_output'         — guidelines specific to tool output content
--   'assistant_reasoning' — guidelines specific to assistant reasoning
--   'user_context'        — guidelines specific to user-provided context
--
-- For compression_failure_pairs category captures what type of content was lost.

ALTER TABLE compression_failure_pairs ADD COLUMN category TEXT NOT NULL DEFAULT 'unknown';

CREATE INDEX IF NOT EXISTS idx_failure_pairs_category
    ON compression_failure_pairs(category, used_in_update);

-- Recreate compression_guidelines with UNIQUE(version, category) constraint.
-- The existing UNIQUE(version) from migration 033 must become UNIQUE(version, category)
-- so different categories can share the same version sequence space.
PRAGMA foreign_keys = OFF;

CREATE TABLE compression_guidelines_new (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    version         INTEGER NOT NULL DEFAULT 1,
    category        TEXT    NOT NULL DEFAULT 'global',
    guidelines      TEXT    NOT NULL DEFAULT '',
    token_count     INTEGER NOT NULL DEFAULT 0,
    conversation_id INTEGER REFERENCES conversations(id) ON DELETE CASCADE,
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(version, category)
);

INSERT INTO compression_guidelines_new
    (id, version, category, guidelines, token_count, conversation_id, created_at)
SELECT id, version, 'global', guidelines, token_count, conversation_id, created_at
FROM compression_guidelines;

DROP TABLE compression_guidelines;
ALTER TABLE compression_guidelines_new RENAME TO compression_guidelines;

CREATE INDEX IF NOT EXISTS idx_compression_guidelines_version
    ON compression_guidelines(version DESC);

CREATE INDEX IF NOT EXISTS idx_compression_guidelines_category
    ON compression_guidelines(category, version DESC);

CREATE INDEX IF NOT EXISTS idx_compression_guidelines_conversation
    ON compression_guidelines(conversation_id);

PRAGMA foreign_keys = ON;
