CREATE TABLE IF NOT EXISTS learned_preferences (
    id               BIGSERIAL PRIMARY KEY,
    preference_key   TEXT NOT NULL UNIQUE,
    preference_value TEXT NOT NULL,
    confidence       DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    evidence_count   INTEGER NOT NULL DEFAULT 0,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
