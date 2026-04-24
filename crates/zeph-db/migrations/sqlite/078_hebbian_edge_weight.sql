-- Migration 077: HL-F1 Hebbian edge weight (#3344) + MM-F4 summaries range index.
-- SQLITE-ONLY. Postgres migration path has a pre-existing 069..076 drift that
-- must be resolved separately before a Postgres counterpart can land.

-- HL-F1: Hebbian reinforcement weight on graph edges. Default 1.0 backfills
-- existing rows; new edges inherit the default (INSERTs need not enumerate it).
ALTER TABLE graph_edges ADD COLUMN weight REAL NOT NULL DEFAULT 1.0;

-- MM-F4: Support index for filter_out_preserved_episode_ids range probes.
-- Partial index: many summaries may have NULL message-id range.
CREATE INDEX IF NOT EXISTS idx_summaries_message_range
    ON summaries(first_message_id, last_message_id)
    WHERE first_message_id IS NOT NULL AND last_message_id IS NOT NULL;
