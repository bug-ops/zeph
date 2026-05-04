-- TACO self-evolving compression rules.
-- Rules apply regex patterns to tool outputs with optional glob matching on tool name.
CREATE TABLE IF NOT EXISTS compression_rules (
    id                    TEXT PRIMARY KEY,
    tool_glob             TEXT,
    pattern               TEXT NOT NULL,
    replacement_template  TEXT NOT NULL,
    hit_count             INTEGER NOT NULL DEFAULT 0,
    source                TEXT NOT NULL DEFAULT 'operator'
                          CHECK (source IN ('operator','llm-evolved')),
    created_at            TEXT NOT NULL,
    UNIQUE(tool_glob, pattern)
);
CREATE INDEX IF NOT EXISTS idx_compression_rules_hits
    ON compression_rules(hit_count DESC);
CREATE INDEX IF NOT EXISTS idx_compression_rules_source
    ON compression_rules(source);
