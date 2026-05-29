-- SYNAPSE multi-timescale synaptic variables (#3709) and turn-level provenance (#3710).
-- confidence_fast: high-plasticity variable tracking recent evidence.
-- confidence_slow: high-retention variable integrating fast over time.
-- turn_index: position within the episode at which this edge was first committed.

ALTER TABLE graph_edges ADD COLUMN confidence_fast REAL NOT NULL DEFAULT 1.0;
ALTER TABLE graph_edges ADD COLUMN confidence_slow REAL NOT NULL DEFAULT 1.0;
ALTER TABLE graph_edges ADD COLUMN turn_index INTEGER;

-- Backfill existing rows so they reflect actual confidence rather than the DEFAULT 1.0.
UPDATE graph_edges SET confidence_fast = confidence, confidence_slow = confidence WHERE confidence_fast = 1.0;
