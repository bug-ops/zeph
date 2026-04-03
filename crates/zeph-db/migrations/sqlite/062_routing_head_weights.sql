-- SkillOrchestra: RL routing head weights (singleton row).
-- Loaded once at agent startup; persisted after each training step (debounced).
-- SINGLE-INSTANCE ONLY: concurrent agents sharing this DB will overwrite each other's weights.
CREATE TABLE IF NOT EXISTS routing_head_weights (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    embed_dim    INTEGER NOT NULL,
    weights      BLOB NOT NULL,
    baseline     REAL NOT NULL DEFAULT 0.0,
    update_count INTEGER NOT NULL DEFAULT 0,
    updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
