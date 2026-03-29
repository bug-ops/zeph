-- Index on importance_score to speed up retrieval ranking queries.
CREATE INDEX IF NOT EXISTS idx_messages_importance_score ON messages(importance_score);
