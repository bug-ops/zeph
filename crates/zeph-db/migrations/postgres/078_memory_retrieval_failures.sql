-- Memory retrieval failure log for OmniMem self-improvement loop (issue #3576).
-- Records no-hit and low-confidence recall events for closed-loop parameter tuning.
CREATE TABLE IF NOT EXISTS memory_retrieval_failures (
    id                   BIGSERIAL PRIMARY KEY,
    conversation_id      BIGINT,
    turn_index           BIGINT NOT NULL DEFAULT 0,
    failure_type         TEXT NOT NULL
                         CHECK (failure_type IN ('no_hit','low_confidence','timeout','error')),
    retrieval_strategy   TEXT NOT NULL,
    query_text           TEXT NOT NULL,
    query_len            BIGINT NOT NULL DEFAULT 0,
    top_score            REAL,
    confidence_threshold REAL,
    result_count         BIGINT NOT NULL DEFAULT 0,
    latency_ms           BIGINT NOT NULL DEFAULT 0,
    edge_types           TEXT,
    error_context        TEXT,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_mrf_type     ON memory_retrieval_failures(failure_type);
CREATE INDEX IF NOT EXISTS idx_mrf_strategy ON memory_retrieval_failures(retrieval_strategy);
CREATE INDEX IF NOT EXISTS idx_mrf_created  ON memory_retrieval_failures(created_at);
