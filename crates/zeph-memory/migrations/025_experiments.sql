CREATE TABLE IF NOT EXISTS experiment_results (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT    NOT NULL,
    parameter       TEXT    NOT NULL,
    value_json      TEXT    NOT NULL,
    baseline_score  REAL    NOT NULL,
    candidate_score REAL    NOT NULL,
    delta           REAL    NOT NULL,
    latency_ms      INTEGER NOT NULL,
    tokens_used     INTEGER NOT NULL,
    accepted        INTEGER NOT NULL DEFAULT 0,
    source          TEXT    NOT NULL DEFAULT 'manual',
    created_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_experiment_results_session   ON experiment_results(session_id);
CREATE INDEX idx_experiment_results_accepted  ON experiment_results(accepted);
CREATE INDEX idx_experiment_results_parameter ON experiment_results(parameter);
