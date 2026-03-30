-- Migration 055: RL admission training data tables (Postgres)

CREATE TABLE IF NOT EXISTS admission_training_data (
    id              BIGSERIAL PRIMARY KEY,
    message_id      BIGINT,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    content_hash    TEXT   NOT NULL,
    role            TEXT   NOT NULL,
    composite_score REAL   NOT NULL,
    was_admitted    INTEGER NOT NULL DEFAULT 0,
    was_recalled    INTEGER NOT NULL DEFAULT 0,
    features_json   TEXT   NOT NULL DEFAULT '[]',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_admission_training_recalled
    ON admission_training_data(was_recalled);
CREATE INDEX IF NOT EXISTS idx_admission_training_message
    ON admission_training_data(message_id);
CREATE INDEX IF NOT EXISTS idx_admission_training_conversation
    ON admission_training_data(conversation_id, created_at);

CREATE TABLE IF NOT EXISTS admission_rl_weights (
    id           BIGSERIAL PRIMARY KEY,
    weights_json TEXT    NOT NULL,
    sample_count INTEGER NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
