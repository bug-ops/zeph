-- Migration 042: AOI three-layer memory tiers.
ALTER TABLE messages ADD COLUMN tier TEXT NOT NULL DEFAULT 'episodic';
ALTER TABLE messages ADD COLUMN promotion_timestamp BIGINT;
ALTER TABLE messages ADD COLUMN session_count INTEGER NOT NULL DEFAULT 0;

-- Index for sweep queries: find episodic messages eligible for promotion.
CREATE INDEX IF NOT EXISTS idx_messages_tier ON messages(tier) WHERE deleted_at IS NULL;

-- Partial index for semantic-only queries in memory_search.
CREATE INDEX IF NOT EXISTS idx_messages_tier_semantic ON messages(tier)
    WHERE tier = 'semantic' AND deleted_at IS NULL;
