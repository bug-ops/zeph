-- Migration 075: APEX-MEM append-only MAGMA edge store.
--
-- Adds supersedes pointer and canonical_relation columns to graph_edges.
-- Adds partial unique index for active head-of-chain.
-- Adds edge_reassertions table for byte-identical re-assertions (FR-015).
-- Retains uq_graph_edges_active for rollback safety (enabled=false path).
--
-- NOTE: sqlx migration runner wraps each migration in a transaction automatically.
-- The spec requires BEGIN IMMEDIATE semantics; this is satisfied by the migration runner.
-- No explicit transaction wrapper here to avoid "cannot start a transaction within a
-- transaction" SQLite errors during automated migration runs.

ALTER TABLE graph_edges ADD COLUMN supersedes INTEGER REFERENCES graph_edges(id);
ALTER TABLE graph_edges ADD COLUMN canonical_relation TEXT;

-- Backfill: use raw relation as canonical for all existing rows (idempotent).
UPDATE graph_edges SET canonical_relation = relation WHERE canonical_relation IS NULL;

-- Partial unique index: at most one active head per (source, target, canonical_relation, edge_type).
-- Coexists with uq_graph_edges_active (retained for rollback; legacy writes satisfy both).
CREATE UNIQUE INDEX IF NOT EXISTS uq_graph_edges_active_head
    ON graph_edges(source_entity_id, target_entity_id, canonical_relation, edge_type)
    WHERE valid_to IS NULL AND expired_at IS NULL;

-- Index for walking supersedes chains.
CREATE INDEX IF NOT EXISTS idx_edges_supersedes ON graph_edges(supersedes);

-- Index for head-of-chain queries ordered by recency.
-- NOTE: SQLite ignores DESC in non-expression indexes unless ORDER BY is used at query time.
-- Queries that use this index must include ORDER BY created_at DESC explicitly.
CREATE INDEX IF NOT EXISTS idx_edges_head_active
    ON graph_edges(source_entity_id, canonical_relation, edge_type, created_at)
    WHERE valid_to IS NULL AND expired_at IS NULL;

-- Reassertion events: byte-identical re-assertions that do not insert a new edge (FR-015).
-- episode_id is nullable: callers with no episode context store NULL.
CREATE TABLE IF NOT EXISTS edge_reassertions (
    id           INTEGER PRIMARY KEY,
    head_edge_id INTEGER NOT NULL REFERENCES graph_edges(id),
    asserted_at  INTEGER NOT NULL,
    episode_id   TEXT,
    confidence   REAL    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_reassertions_head
    ON edge_reassertions(head_edge_id, asserted_at DESC);
