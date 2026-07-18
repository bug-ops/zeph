-- no-transaction
-- Migration 112 (#6362): add 'canceled' to durable_executions.status CHECK.
-- durable_executions is a PARENT of durable_journal/promises/timers (FK, default NO ACTION).
-- SQLite cannot alter a CHECK in place. The rebuild must run with foreign_keys OFF, which is only
-- possible OUTSIDE a transaction — hence `-- no-transaction`. The explicit BEGIN/COMMIT restores
-- atomicity so a crash mid-rebuild rolls back cleanly (DROP TABLE IF EXISTS handles a prior partial).
PRAGMA foreign_keys = OFF;
BEGIN;
DROP TABLE IF EXISTS durable_executions_new;
CREATE TABLE durable_executions_new (
    execution_id TEXT    PRIMARY KEY,
    kind         TEXT    NOT NULL,
    status       TEXT    NOT NULL CHECK(status IN ('running', 'completed', 'failed', 'aborted', 'canceled')),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    finalized_at INTEGER
);
INSERT INTO durable_executions_new SELECT * FROM durable_executions;
DROP TABLE durable_executions;
ALTER TABLE durable_executions_new RENAME TO durable_executions;
CREATE INDEX idx_durable_exec_status_time ON durable_executions(status, finalized_at);
COMMIT;
PRAGMA foreign_keys = ON;
