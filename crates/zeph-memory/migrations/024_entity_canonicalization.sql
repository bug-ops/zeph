-- @@DISABLE_TRANSACTION
-- Migration 024: Entity canonicalization with alias table.
-- Adds canonical_name column to graph_entities and creates graph_entity_aliases lookup table.
--
-- @@DISABLE_TRANSACTION is required because SQLite forbids changing PRAGMA foreign_keys
-- inside a transaction (it is silently ignored). Running outside a transaction allows the
-- PRAGMA guards below to take effect, preventing ON DELETE CASCADE from graph_edges
-- wiping all edges when graph_entities is DROPped and recreated.
PRAGMA foreign_keys = OFF;

-- 1. Add canonical_name column (nullable first so ALTER TABLE succeeds, then backfill).
ALTER TABLE graph_entities ADD COLUMN canonical_name TEXT;
UPDATE graph_entities SET canonical_name = name WHERE canonical_name IS NULL;

-- 2. Drop the old UNIQUE(name, entity_type) constraint by recreating the table.
--    SQLite does not support DROP CONSTRAINT, so we use the standard
--    create-new → copy → drop-old → rename pattern.
--    NOTE: DROP TABLE also drops FTS5 triggers from migration 023. They are recreated below.
CREATE TABLE graph_entities_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    canonical_name TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    summary TEXT,
    first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    qdrant_point_id TEXT,
    UNIQUE(canonical_name, entity_type)
);

INSERT INTO graph_entities_new
    (id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id)
SELECT id, name, COALESCE(canonical_name, name), entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
FROM graph_entities;

DROP TABLE graph_entities;
ALTER TABLE graph_entities_new RENAME TO graph_entities;

-- Recreate indexes on the renamed table.
CREATE INDEX IF NOT EXISTS idx_graph_entities_name ON graph_entities(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_graph_entities_canonical ON graph_entities(canonical_name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_graph_entities_type ON graph_entities(entity_type);
CREATE INDEX IF NOT EXISTS idx_graph_entities_last_seen ON graph_entities(last_seen_at);

-- Safety net: recreate graph_edges indexes in case they were affected by the table drop.
CREATE INDEX IF NOT EXISTS idx_graph_edges_source ON graph_edges(source_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_target ON graph_edges(target_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_valid ON graph_edges(valid_to) WHERE valid_to IS NULL;

-- 3. Rebuild FTS5 triggers dropped when graph_entities was dropped (originally from migration 023).
DROP TRIGGER IF EXISTS graph_entities_fts_insert;
DROP TRIGGER IF EXISTS graph_entities_fts_delete;
DROP TRIGGER IF EXISTS graph_entities_fts_update;

CREATE TRIGGER IF NOT EXISTS graph_entities_fts_insert AFTER INSERT ON graph_entities
BEGIN
    INSERT INTO graph_entities_fts(rowid, name, summary)
        VALUES (new.id, new.name, COALESCE(new.summary, ''));
END;

CREATE TRIGGER IF NOT EXISTS graph_entities_fts_delete AFTER DELETE ON graph_entities
BEGIN
    INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary)
        VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
END;

CREATE TRIGGER IF NOT EXISTS graph_entities_fts_update AFTER UPDATE ON graph_entities
BEGIN
    INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary)
        VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
    INSERT INTO graph_entities_fts(rowid, name, summary)
        VALUES (new.id, new.name, COALESCE(new.summary, ''));
END;

-- Rebuild FTS5 index content from the new table.
INSERT INTO graph_entities_fts(graph_entities_fts) VALUES('rebuild');

-- 4. Alias table: maps variant surface forms to canonical entity IDs.
CREATE TABLE IF NOT EXISTS graph_entity_aliases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    alias_name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(alias_name, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_entity_aliases_name
    ON graph_entity_aliases(alias_name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_graph_entity_aliases_entity
    ON graph_entity_aliases(entity_id);

-- 5. Seed initial aliases from existing entity names (each entity's name becomes its first alias).
INSERT OR IGNORE INTO graph_entity_aliases (entity_id, alias_name)
SELECT id, name FROM graph_entities;

PRAGMA foreign_keys = ON;
