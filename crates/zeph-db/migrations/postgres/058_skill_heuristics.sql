-- ERL: experiential heuristics extracted from successful tasks.
CREATE TABLE IF NOT EXISTS skill_heuristics (
    id             BIGSERIAL PRIMARY KEY,
    skill_name     TEXT,
    heuristic_text TEXT NOT NULL,
    confidence     DOUBLE PRECISION NOT NULL DEFAULT 0.5,
    use_count      BIGINT NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_skill_heuristics_name_conf ON skill_heuristics(skill_name, confidence DESC);
