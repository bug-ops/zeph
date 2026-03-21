-- Migration 042: AOI three-layer memory tiers (#1839).
--
-- Adds tier classification to messages:
--   'episodic' (default) — session-bound messages
--   'semantic'           — cross-session distilled facts, promoted from episodic
-- Working tier is virtual (current context window), not stored as a column.
--
-- session_count starts at 0 so existing messages are not artificially credited
-- with one session toward the promotion threshold.
--
-- SQLite ALTER TABLE ADD COLUMN with constant DEFAULT is O(1) — no table rewrite.

ALTER TABLE messages ADD COLUMN tier TEXT NOT NULL DEFAULT 'episodic';
ALTER TABLE messages ADD COLUMN promotion_timestamp INTEGER;
ALTER TABLE messages ADD COLUMN session_count INTEGER NOT NULL DEFAULT 0;

-- Index for sweep queries: find episodic messages eligible for promotion.
CREATE INDEX IF NOT EXISTS idx_messages_tier ON messages(tier) WHERE deleted_at IS NULL;

-- Partial index for semantic-only queries in memory_search.
CREATE INDEX IF NOT EXISTS idx_messages_tier_semantic ON messages(tier)
    WHERE tier = 'semantic' AND deleted_at IS NULL;
