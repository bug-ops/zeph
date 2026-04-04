-- Training data for the compression quality predictor (#2460).
CREATE TABLE IF NOT EXISTS compression_predictor_training (
    id                    BIGSERIAL PRIMARY KEY,
    conversation_id       BIGINT  NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    compression_ratio     REAL    NOT NULL,
    message_count         INTEGER NOT NULL,
    avg_message_length    REAL    NOT NULL,
    tool_output_fraction  REAL    NOT NULL,
    probe_score           REAL    NOT NULL,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_compression_predictor_created
    ON compression_predictor_training(created_at);

-- Persisted model weights for the compression predictor (singleton row).
CREATE TABLE IF NOT EXISTS compression_predictor_weights (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    weights_json TEXT    NOT NULL DEFAULT '{}',
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
