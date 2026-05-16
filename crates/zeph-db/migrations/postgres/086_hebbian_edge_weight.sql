-- Migration 086: HL-F1 Hebbian edge weight + MM-F4 summaries range index (PostgreSQL).
-- Adapted from SQLite migration 078.

-- HL-F1: Hebbian reinforcement weight on graph edges.
ALTER TABLE graph_edges ADD COLUMN weight REAL NOT NULL DEFAULT 1.0;

-- MM-F4: Support index for filter_out_preserved_episode_ids range probes.
CREATE INDEX IF NOT EXISTS idx_summaries_message_range
    ON summaries(first_message_id, last_message_id)
    WHERE first_message_id IS NOT NULL AND last_message_id IS NOT NULL;
