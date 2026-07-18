-- Migration 112 (#6362): add 'canceled' to durable_executions.status CHECK.
-- Postgres allows an in-place constraint swap (unlike SQLite, no table rebuild needed).
ALTER TABLE durable_executions DROP CONSTRAINT durable_executions_status_check;
ALTER TABLE durable_executions ADD CONSTRAINT durable_executions_status_check
    CHECK (status IN ('running', 'completed', 'failed', 'aborted', 'canceled'));
