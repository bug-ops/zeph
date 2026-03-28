-- Write-time importance score for retrieval ranking.
-- Range [0.0, 1.0], default 0.5 (neutral) for backward compatibility.
ALTER TABLE messages ADD COLUMN importance_score DOUBLE PRECISION NOT NULL DEFAULT 0.5;
