CREATE TABLE IF NOT EXISTS experiment_results (
    id              BIGSERIAL PRIMARY KEY,
    session_id      TEXT    NOT NULL,
    parameter       TEXT    NOT NULL,
    value_json      TEXT    NOT NULL,
    baseline_score  DOUBLE PRECISION NOT NULL,
    candidate_score DOUBLE PRECISION NOT NULL,
    delta           DOUBLE PRECISION NOT NULL,
    latency_ms      INTEGER NOT NULL,
    tokens_used     INTEGER NOT NULL,
    accepted        BOOLEAN NOT NULL DEFAULT FALSE,
    source          TEXT    NOT NULL DEFAULT 'manual',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_experiment_results_session   ON experiment_results(session_id);
CREATE INDEX idx_experiment_results_accepted  ON experiment_results(accepted);
CREATE INDEX idx_experiment_results_parameter ON experiment_results(parameter);
