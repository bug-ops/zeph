-- TiMem leaf-starvation guard (#6393): singleton leaves that never cluster kept getting
-- re-loaded as the oldest `parent_id IS NULL` rows on every sweep, permanently blocking
-- newer leaves from ever being considered once more than `batch_size` singletons accumulate.
-- `consolidation_attempts`/`last_attempted_at` let the load queries in memory_tree.rs order by
-- true LRU (`COALESCE(last_attempted_at, created_at) ASC`) instead of strict oldest-first: a
-- leaf that keeps losing the race has its touch time frozen in the past while every competing
-- leaf's touch time refreshes to "now" each time it's tried, so the frozen leaf's priority
-- strictly increases until it wins — a bounded-wait guarantee even under a sustained stream of
-- brand-new arrivals, not just a bias (see memory_tree.rs `load_tree_leaves_unconsolidated`).
ALTER TABLE memory_tree ADD COLUMN consolidation_attempts BIGINT NOT NULL DEFAULT 0;
ALTER TABLE memory_tree ADD COLUMN last_attempted_at TIMESTAMPTZ;

-- Serves the `ORDER BY COALESCE(last_attempted_at, created_at) ASC` load queries at any level.
CREATE INDEX IF NOT EXISTS idx_memory_tree_unconsolidated_attempts
    ON memory_tree(level, parent_id, COALESCE(last_attempted_at, created_at))
    WHERE parent_id IS NULL;
