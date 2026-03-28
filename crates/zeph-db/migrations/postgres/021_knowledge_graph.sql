-- Knowledge graph: entities, edges, communities for graph-based memory.

CREATE TABLE IF NOT EXISTS graph_entities (
    id              BIGSERIAL PRIMARY KEY,
    name            TEXT NOT NULL,
    entity_type     TEXT NOT NULL,
    summary         TEXT,
    first_seen_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    qdrant_point_id TEXT,
    UNIQUE(name, entity_type)
);

CREATE TABLE IF NOT EXISTS graph_edges (
    id               BIGSERIAL PRIMARY KEY,
    source_entity_id BIGINT NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    target_entity_id BIGINT NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    relation         TEXT NOT NULL,
    fact             TEXT NOT NULL,
    confidence       DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    valid_from       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_to         TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expired_at       TIMESTAMPTZ,
    episode_id       BIGINT REFERENCES messages(id) ON DELETE SET NULL,
    qdrant_point_id  TEXT
);

CREATE TABLE IF NOT EXISTS graph_communities (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT NOT NULL,
    summary    TEXT NOT NULL,
    entity_ids TEXT NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(name)
);

CREATE TABLE IF NOT EXISTS graph_metadata (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_graph_edges_source ON graph_edges(source_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_target ON graph_edges(target_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_valid ON graph_edges(valid_to)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS idx_graph_entities_name ON graph_entities(LOWER(name));
CREATE INDEX IF NOT EXISTS idx_graph_entities_type ON graph_entities(entity_type);
CREATE INDEX IF NOT EXISTS idx_graph_entities_last_seen ON graph_entities(last_seen_at);

ALTER TABLE messages ADD COLUMN graph_processed BOOLEAN NOT NULL DEFAULT FALSE;
