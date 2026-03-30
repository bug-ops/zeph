-- ARISE/STEM: tool usage log for pattern detection.
CREATE TABLE IF NOT EXISTS skill_usage_log (
    id              BIGSERIAL PRIMARY KEY,
    tool_sequence   TEXT NOT NULL,
    sequence_hash   TEXT NOT NULL,
    context_hash    TEXT NOT NULL,
    outcome         TEXT NOT NULL CHECK (outcome IN ('success', 'failure')),
    conversation_id BIGINT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_skill_usage_log_seq_hash ON skill_usage_log(sequence_hash);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_created ON skill_usage_log(created_at);
