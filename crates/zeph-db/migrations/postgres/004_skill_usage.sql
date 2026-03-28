CREATE TABLE skill_usage (
    id              BIGSERIAL PRIMARY KEY,
    skill_name      TEXT NOT NULL UNIQUE,
    invocation_count INTEGER NOT NULL DEFAULT 0,
    last_used_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
