CREATE TABLE IF NOT EXISTS plan_cache (
    id               TEXT PRIMARY KEY,
    goal_hash        TEXT NOT NULL UNIQUE,
    goal_text        TEXT NOT NULL,
    template         TEXT NOT NULL,
    task_count       INTEGER NOT NULL,
    success_count    INTEGER NOT NULL DEFAULT 1,
    adapted_count    INTEGER NOT NULL DEFAULT 0,
    embedding        BYTEA,
    embedding_model  TEXT,
    created_at       BIGINT NOT NULL,
    last_accessed_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_plan_cache_last_accessed
    ON plan_cache(last_accessed_at);
