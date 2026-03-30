-- Migration 054: Memex archive type + ACON per-category guidelines (Postgres)

ALTER TABLE tool_overflow ADD COLUMN archive_type TEXT NOT NULL DEFAULT 'overflow';

CREATE INDEX IF NOT EXISTS idx_tool_overflow_archive_type
    ON tool_overflow(archive_type);

ALTER TABLE compression_failure_pairs ADD COLUMN category TEXT NOT NULL DEFAULT 'unknown';

CREATE INDEX IF NOT EXISTS idx_failure_pairs_category
    ON compression_failure_pairs(category, used_in_update);

-- Postgres supports ADD CONSTRAINT on existing tables.
ALTER TABLE compression_guidelines ADD COLUMN category TEXT NOT NULL DEFAULT 'global';

-- Drop the old UNIQUE(version) constraint (name from migration 033).
ALTER TABLE compression_guidelines DROP CONSTRAINT IF EXISTS compression_guidelines_version_key;

ALTER TABLE compression_guidelines ADD CONSTRAINT compression_guidelines_version_category_key
    UNIQUE(version, category);

CREATE INDEX IF NOT EXISTS idx_compression_guidelines_category
    ON compression_guidelines(category, version DESC);
