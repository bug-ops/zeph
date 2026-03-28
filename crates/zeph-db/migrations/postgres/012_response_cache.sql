CREATE TABLE IF NOT EXISTS response_cache (
    cache_key  TEXT PRIMARY KEY,
    response   TEXT NOT NULL,
    model      TEXT NOT NULL,
    created_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM NOW())::BIGINT,
    expires_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_response_cache_expires ON response_cache(expires_at);
