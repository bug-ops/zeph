CREATE TABLE IF NOT EXISTS implicit_conflict_candidates (
    id          INTEGER PRIMARY KEY,
    edge_a_id   INTEGER NOT NULL REFERENCES graph_edges(id),
    edge_b_id   INTEGER NOT NULL REFERENCES graph_edges(id),
    similarity  REAL    NOT NULL,
    method      TEXT    NOT NULL,
    status      TEXT    NOT NULL DEFAULT 'pending',
    resolution  TEXT,
    created_at  INTEGER NOT NULL,
    resolved_at INTEGER,
    expires_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_icc_edge_a ON implicit_conflict_candidates(edge_a_id);
CREATE INDEX IF NOT EXISTS idx_icc_edge_b ON implicit_conflict_candidates(edge_b_id);
CREATE INDEX IF NOT EXISTS idx_icc_status  ON implicit_conflict_candidates(status, expires_at);
