-- FTS5 virtual table for keyword search over graph entities.
-- Indexes name and summary; content-sync keeps it in lockstep with graph_entities.
-- Uses unicode61 tokenizer with full Unicode normalization (remove_diacritics=2)
-- to support non-Latin entity names. ASCII-only tokenizer would miss those.
CREATE VIRTUAL TABLE IF NOT EXISTS graph_entities_fts USING fts5(
    name,
    summary,
    content='graph_entities',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

-- Backfill existing entities into the FTS index.
-- COALESCE(summary, '') because FTS5 does not index NULL; coalescing to empty
-- string ensures name-only matches still work for entities without a summary.
INSERT INTO graph_entities_fts(rowid, name, summary)
    SELECT id, name, COALESCE(summary, '') FROM graph_entities;

-- Keep FTS index in sync on INSERT.
CREATE TRIGGER IF NOT EXISTS graph_entities_fts_insert AFTER INSERT ON graph_entities
BEGIN
    INSERT INTO graph_entities_fts(rowid, name, summary)
        VALUES (new.id, new.name, COALESCE(new.summary, ''));
END;

-- Keep FTS index in sync on DELETE.
-- Currently unused (no delete_entity method exists), but required for FTS
-- consistency if entity deletion is added in the future.
CREATE TRIGGER IF NOT EXISTS graph_entities_fts_delete AFTER DELETE ON graph_entities
BEGIN
    INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary)
        VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
END;

-- Keep FTS index in sync on UPDATE.
-- upsert_entity uses INSERT...ON CONFLICT DO UPDATE, which fires UPDATE triggers.
-- The trigger fires on any column change (including last_seen_at); this is
-- acceptable at current write volumes. For high-write scenarios, add a WHEN
-- clause to filter on name/summary changes only.
-- TODO: optimize for high-write-volume: WHEN (old.name != new.name OR COALESCE(old.summary,'') != COALESCE(new.summary,''))
CREATE TRIGGER IF NOT EXISTS graph_entities_fts_update AFTER UPDATE ON graph_entities
BEGIN
    INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary)
        VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
    INSERT INTO graph_entities_fts(rowid, name, summary)
        VALUES (new.id, new.name, COALESCE(new.summary, ''));
END;
