-- Index for forgetting sweep: find low-importance non-deleted, non-consolidated messages efficiently.
CREATE INDEX IF NOT EXISTS idx_messages_forgetting
    ON messages(importance_score, consolidated)
    WHERE deleted_at IS NULL AND consolidated = 0;
