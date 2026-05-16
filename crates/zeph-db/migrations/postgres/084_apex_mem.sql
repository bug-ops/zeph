-- Migration 084: APEX-MEM append-only MAGMA edge store (PostgreSQL).
--
-- Adapted from SQLite migration 075. Adds supersedes pointer and canonical_relation
-- columns to graph_edges, partial unique index for active head-of-chain,
-- and edge_reassertions table for byte-identical re-assertions (FR-015).

ALTER TABLE graph_edges ADD COLUMN supersedes BIGINT REFERENCES graph_edges(id);
ALTER TABLE graph_edges ADD COLUMN canonical_relation TEXT;

-- Backfill: use raw relation as canonical for all existing rows (idempotent).
UPDATE graph_edges SET canonical_relation = relation WHERE canonical_relation IS NULL;

-- Partial unique index: at most one active head per (source, target, canonical_relation, edge_type).
CREATE UNIQUE INDEX IF NOT EXISTS uq_graph_edges_active_head
    ON graph_edges(source_entity_id, target_entity_id, canonical_relation, edge_type)
    WHERE valid_to IS NULL AND expired_at IS NULL;

-- Index for walking supersedes chains.
CREATE INDEX IF NOT EXISTS idx_edges_supersedes ON graph_edges(supersedes);

-- Index for head-of-chain queries ordered by recency.
CREATE INDEX IF NOT EXISTS idx_edges_head_active
    ON graph_edges(source_entity_id, canonical_relation, edge_type, created_at)
    WHERE valid_to IS NULL AND expired_at IS NULL;

-- Reassertion events: byte-identical re-assertions that do not insert a new edge (FR-015).
-- episode_id is nullable: callers with no episode context store NULL.
CREATE TABLE IF NOT EXISTS edge_reassertions (
    id           BIGSERIAL PRIMARY KEY,
    head_edge_id BIGINT NOT NULL REFERENCES graph_edges(id),
    asserted_at  BIGINT NOT NULL,
    episode_id   TEXT,
    confidence   REAL   NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_reassertions_head
    ON edge_reassertions(head_edge_id, asserted_at DESC);
