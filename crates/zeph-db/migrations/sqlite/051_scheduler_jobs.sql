-- Scheduled jobs table for the cron-based task scheduler.
CREATE TABLE IF NOT EXISTS scheduled_jobs (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT    NOT NULL UNIQUE,
    cron_expr TEXT    NOT NULL DEFAULT '',
    kind      TEXT    NOT NULL,
    last_run  TEXT,
    next_run  TEXT,
    status    TEXT    NOT NULL DEFAULT 'pending',
    task_mode TEXT    NOT NULL DEFAULT 'periodic',
    run_at    TEXT
);
