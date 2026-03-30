-- Migration 055: RL admission training data tables
--
-- admission_training_data: records every message seen by A-MAC (admitted and rejected)
-- so the RL logistic regression model can learn from both classes (fix C3 survivorship bias).
--
-- was_admitted: 1 if the message passed admission gate, 0 if rejected
-- was_recalled:  1 if the message was later found via SemanticMemory::recall(), 0 otherwise
-- composite_score: the A-MAC composite score at decision time
-- features_json: JSON-encoded feature vector used for training
CREATE TABLE IF NOT EXISTS admission_training_data (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id      INTEGER,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    content_hash    TEXT    NOT NULL,
    role            TEXT    NOT NULL,
    composite_score REAL    NOT NULL,
    was_admitted    INTEGER NOT NULL DEFAULT 0,
    was_recalled    INTEGER NOT NULL DEFAULT 0,
    features_json   TEXT    NOT NULL DEFAULT '[]',
    created_at      TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_admission_training_recalled
    ON admission_training_data(was_recalled);
CREATE INDEX IF NOT EXISTS idx_admission_training_message
    ON admission_training_data(message_id);
CREATE INDEX IF NOT EXISTS idx_admission_training_conversation
    ON admission_training_data(conversation_id, created_at);

-- admission_rl_weights: persists the trained logistic regression model weights.
-- One row per model version; the highest id is the active model.
CREATE TABLE IF NOT EXISTS admission_rl_weights (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    weights_json TEXT   NOT NULL,
    sample_count INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT   NOT NULL DEFAULT (datetime('now'))
);
