-- PostgreSQL counterpart of sqlite/090_implicit_conflict_candidates.sql; closes the dialect
-- gap where Postgres deployments lacked the implicit_conflict_candidates table used by graph
-- conflict detection (parity defect: #4957). graph_edges(id) is BIGSERIAL on Postgres, so the
-- foreign keys are BIGINT; epoch timestamps are BIGINT (bound as i64) and similarity is
-- DOUBLE PRECISION (bound as f64), mirroring the Rust accessors in graph/implicit_conflict.rs.

CREATE TABLE IF NOT EXISTS implicit_conflict_candidates (
    id          BIGSERIAL PRIMARY KEY,
    edge_a_id   BIGINT NOT NULL REFERENCES graph_edges(id),
    edge_b_id   BIGINT NOT NULL REFERENCES graph_edges(id),
    similarity  DOUBLE PRECISION NOT NULL,
    method      TEXT   NOT NULL,
    status      TEXT   NOT NULL DEFAULT 'pending',
    resolution  TEXT,
    created_at  BIGINT NOT NULL,
    resolved_at BIGINT,
    expires_at  BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_icc_edge_a ON implicit_conflict_candidates(edge_a_id);
CREATE INDEX IF NOT EXISTS idx_icc_edge_b ON implicit_conflict_candidates(edge_b_id);
CREATE INDEX IF NOT EXISTS idx_icc_status ON implicit_conflict_candidates(status, expires_at);
