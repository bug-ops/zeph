-- Migration 079: HL-F3/F4 Hebbian background consolidation (#3345).
-- Adds a `consolidated_at` cooldown column to graph_entities and creates the
-- `graph_rules` table for LLM-distilled consolidation summaries.

-- HL-F3: cooldown marker — NULL means "never consolidated"; epoch seconds once set.
ALTER TABLE graph_entities ADD COLUMN consolidated_at INTEGER;

-- HL-F4: LLM-distilled rules anchored to a high-traffic entity cluster.
CREATE TABLE IF NOT EXISTS graph_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    anchor_entity_id INTEGER NOT NULL REFERENCES graph_entities(id),
    summary TEXT NOT NULL,
    trigger_hint TEXT,
    confidence REAL NOT NULL DEFAULT 0.0,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_graph_rules_anchor  ON graph_rules(anchor_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_rules_created ON graph_rules(created_at);
CREATE INDEX IF NOT EXISTS idx_graph_entities_consolidated
    ON graph_entities(consolidated_at)
    WHERE consolidated_at IS NOT NULL;
