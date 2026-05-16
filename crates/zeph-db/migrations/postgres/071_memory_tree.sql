-- TiMem temporal-hierarchical memory tree (#2262).
-- Stores leaf memories and consolidated summaries in a hierarchy.
CREATE TABLE IF NOT EXISTS memory_tree (
    id              BIGSERIAL PRIMARY KEY,
    level           BIGINT NOT NULL DEFAULT 0,
    parent_id       BIGINT REFERENCES memory_tree(id),
    content         TEXT NOT NULL,
    source_ids      TEXT NOT NULL DEFAULT '[]',
    token_count     BIGINT NOT NULL DEFAULT 0,
    consolidated_at TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_memory_tree_level ON memory_tree(level);
CREATE INDEX IF NOT EXISTS idx_memory_tree_parent ON memory_tree(parent_id);
-- Fast lookup of unconsolidated leaves (level=0, no parent yet).
CREATE INDEX IF NOT EXISTS idx_memory_tree_unconsolidated
    ON memory_tree(level, parent_id)
    WHERE level = 0 AND parent_id IS NULL;

-- Meta tracking for consolidation progress (tree-wide stats only, not per-conversation).
CREATE TABLE IF NOT EXISTS memory_tree_meta (
    id                      BIGINT PRIMARY KEY CHECK(id = 1),
    last_consolidation_at   TIMESTAMPTZ,
    total_consolidations    BIGINT NOT NULL DEFAULT 0,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO memory_tree_meta (id) VALUES (1) ON CONFLICT DO NOTHING;
