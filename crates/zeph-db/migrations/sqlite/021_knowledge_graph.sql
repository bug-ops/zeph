-- Knowledge graph: entities, edges, communities for graph-based memory.
-- Tables are always created (schema presence is independent of feature flag).
-- The feature flag controls whether Rust code reads/writes these tables.

CREATE TABLE IF NOT EXISTS graph_entities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    summary TEXT,
    first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    qdrant_point_id TEXT,
    UNIQUE(name, entity_type)
);

CREATE TABLE IF NOT EXISTS graph_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    target_entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    relation TEXT NOT NULL,
    fact TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    valid_from TEXT NOT NULL DEFAULT (datetime('now')),
    valid_to TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expired_at TEXT,
    episode_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    qdrant_point_id TEXT
);

CREATE TABLE IF NOT EXISTS graph_communities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    summary TEXT NOT NULL,
    entity_ids TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(name)
);

CREATE TABLE IF NOT EXISTS graph_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_graph_edges_source ON graph_edges(source_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_target ON graph_edges(target_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_valid ON graph_edges(valid_to)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS idx_graph_entities_name ON graph_entities(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_graph_entities_type ON graph_entities(entity_type);
CREATE INDEX IF NOT EXISTS idx_graph_entities_last_seen ON graph_entities(last_seen_at);

ALTER TABLE messages ADD COLUMN graph_processed INTEGER NOT NULL DEFAULT 0;
