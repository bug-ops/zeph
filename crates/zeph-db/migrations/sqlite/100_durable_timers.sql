-- Durable execution layer (spec-064, #4944): durable wakes persisted across restarts.
CREATE TABLE durable_timers (
    timer_id     TEXT    PRIMARY KEY,
    execution_id TEXT    NOT NULL REFERENCES durable_executions(execution_id),
    due_at       INTEGER NOT NULL,
    fired        INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);

CREATE INDEX idx_durable_timers_due ON durable_timers(fired, due_at);
