CREATE TABLE skill_versions (
    id             BIGSERIAL PRIMARY KEY,
    skill_name     TEXT NOT NULL,
    version        INTEGER NOT NULL,
    body           TEXT NOT NULL,
    description    TEXT NOT NULL,
    source         TEXT NOT NULL DEFAULT 'manual',
    error_context  TEXT,
    predecessor_id BIGINT REFERENCES skill_versions(id) ON DELETE SET NULL,
    is_active      BOOLEAN NOT NULL DEFAULT FALSE,
    success_count  INTEGER NOT NULL DEFAULT 0,
    failure_count  INTEGER NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX idx_skill_version ON skill_versions(skill_name, version);
CREATE INDEX idx_skill_active ON skill_versions(skill_name, is_active);

CREATE TABLE skill_outcomes (
    id              BIGSERIAL PRIMARY KEY,
    skill_name      TEXT NOT NULL,
    version_id      BIGINT REFERENCES skill_versions(id) ON DELETE SET NULL,
    conversation_id BIGINT,
    outcome         TEXT NOT NULL,
    error_context   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_skill_outcomes_name ON skill_outcomes(skill_name);
