-- SkillOrchestra: RL routing head weights (singleton row).
-- SINGLE-INSTANCE ONLY: concurrent agents sharing this DB will overwrite each other's weights.
CREATE TABLE IF NOT EXISTS routing_head_weights (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    embed_dim    INTEGER NOT NULL,
    weights      BYTEA NOT NULL,
    baseline     DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    update_count BIGINT NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
