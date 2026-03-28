-- Per-server MCP trust scores with asymmetric time decay.
CREATE TABLE IF NOT EXISTS mcp_trust_scores (
    server_id       TEXT PRIMARY KEY NOT NULL,
    score           DOUBLE PRECISION NOT NULL DEFAULT 0.5,
    success_count   INTEGER NOT NULL DEFAULT 0,
    failure_count   INTEGER NOT NULL DEFAULT 0,
    updated_at_secs BIGINT NOT NULL DEFAULT 0
);
