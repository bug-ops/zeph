-- Add UNIQUE(version) constraint to compression_guidelines.
-- SQLite does not support ALTER TABLE ... ADD CONSTRAINT, so we recreate the table.
--
-- The original bug (race in save_compression_guidelines) could have produced rows
-- with duplicate version numbers. To avoid a UNIQUE constraint violation during
-- migration, we keep only the row with the highest rowid per version (i.e. the
-- most recently inserted one in case of duplicates).
PRAGMA foreign_keys = OFF;

CREATE TABLE compression_guidelines_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    version     INTEGER NOT NULL DEFAULT 1,
    guidelines  TEXT    NOT NULL DEFAULT '',
    token_count INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(version)
);

INSERT INTO compression_guidelines_new (id, version, guidelines, token_count, created_at)
    SELECT id, version, guidelines, token_count, created_at
    FROM compression_guidelines
    WHERE rowid IN (
        SELECT MAX(rowid) FROM compression_guidelines GROUP BY version
    );

DROP TABLE compression_guidelines;
ALTER TABLE compression_guidelines_new RENAME TO compression_guidelines;

CREATE INDEX IF NOT EXISTS idx_compression_guidelines_version
    ON compression_guidelines(version DESC);

PRAGMA foreign_keys = ON;
