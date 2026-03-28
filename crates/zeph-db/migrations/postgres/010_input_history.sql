CREATE TABLE IF NOT EXISTS input_history (
    id         BIGSERIAL PRIMARY KEY,
    input      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
