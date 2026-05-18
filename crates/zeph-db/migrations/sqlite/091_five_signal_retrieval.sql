-- Five-signal SYNAPSE retrieval: access frequency tracking and memory tier columns (issue #4374).

CREATE TABLE IF NOT EXISTS fact_access_log (
    id          INTEGER PRIMARY KEY,
    fact_id     INTEGER NOT NULL,
    fact_type   TEXT    NOT NULL,
    session_id  TEXT    NOT NULL,
    accessed_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fact_access_fact         ON fact_access_log(fact_id,              accessed_at DESC);
CREATE INDEX IF NOT EXISTS idx_fact_access_session      ON fact_access_log(session_id,           accessed_at DESC);
-- Composite index: covers the GROUP BY query (session_id = ? AND fact_id IN (...)).
CREATE INDEX IF NOT EXISTS idx_fact_access_session_fact ON fact_access_log(session_id, fact_id);

ALTER TABLE messages ADD COLUMN memory_tier     TEXT    DEFAULT 'episodic';
ALTER TABLE messages ADD COLUMN qdrant_promoted INTEGER DEFAULT 0;
