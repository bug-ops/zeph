-- Five-signal SYNAPSE retrieval: access frequency tracking and memory tier columns (issue #4374).
-- PostgreSQL counterpart of sqlite/091_five_signal_retrieval.sql; closes the dialect gap
-- where Postgres deployments lacked the fact_access_log table and the messages tier columns
-- (parity defect: #4957). Integer columns are BIGINT because the Rust accessors read them as
-- i64 (e.g. row.get::<i64, _>("qdrant_promoted")); an INTEGER/INT4 column would fail i64 decode.

CREATE TABLE IF NOT EXISTS fact_access_log (
    id          BIGSERIAL PRIMARY KEY,
    fact_id     BIGINT NOT NULL,
    fact_type   TEXT   NOT NULL,
    session_id  TEXT   NOT NULL,
    accessed_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fact_access_fact         ON fact_access_log(fact_id,    accessed_at DESC);
CREATE INDEX IF NOT EXISTS idx_fact_access_session      ON fact_access_log(session_id, accessed_at DESC);
-- Composite index: covers the GROUP BY query (session_id = ? AND fact_id IN (...)).
CREATE INDEX IF NOT EXISTS idx_fact_access_session_fact ON fact_access_log(session_id, fact_id);

ALTER TABLE messages ADD COLUMN IF NOT EXISTS memory_tier     TEXT   DEFAULT 'episodic';
ALTER TABLE messages ADD COLUMN IF NOT EXISTS qdrant_promoted BIGINT DEFAULT 0;
