-- Durable execution layer (spec-064, #4944): one row per durable execution.
-- Applied via zeph_db::run_migrations against the dedicated durable.db pool (INV-14).
-- The owning zeph-durable crate holds no .sql files and no sqlx::migrate!.
CREATE TABLE durable_executions (
    execution_id TEXT    PRIMARY KEY,
    kind         TEXT    NOT NULL,
    status       TEXT    NOT NULL CHECK(status IN ('running', 'completed', 'failed', 'aborted')),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    finalized_at INTEGER                  -- NULL until terminal; drives retention.
);

CREATE INDEX idx_durable_exec_status_time ON durable_executions(status, finalized_at);
