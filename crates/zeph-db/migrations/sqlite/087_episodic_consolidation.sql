-- Migration: 087_episodic_consolidation.sql
-- Episodic-to-semantic consolidation daemon tracking (issue #3799).

-- Track which episodic events have been processed by the consolidation daemon.
ALTER TABLE episodic_events ADD COLUMN consolidated_at INTEGER;

-- Consolidated facts promoted from episodic events.
-- Facts land in Qdrant `zeph_key_facts` collection AND in this SQLite table
-- for provenance tracking and dedup queries.
CREATE TABLE IF NOT EXISTS consolidated_facts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_text        TEXT NOT NULL,
    source           TEXT NOT NULL DEFAULT 'episodic_consolidation',
    cognitive_weight REAL NOT NULL DEFAULT 0.0,
    created_at       INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Link table: which episodic events contributed to which consolidated fact.
CREATE TABLE IF NOT EXISTS consolidated_fact_sources (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_id  INTEGER NOT NULL REFERENCES consolidated_facts(id),
    event_id INTEGER NOT NULL REFERENCES episodic_events(id),
    UNIQUE(fact_id, event_id)
);

CREATE INDEX IF NOT EXISTS idx_episodic_events_consolidated ON episodic_events(consolidated_at);
CREATE INDEX IF NOT EXISTS idx_episodic_events_created ON episodic_events(created_at);
CREATE INDEX IF NOT EXISTS idx_consolidated_facts_source ON consolidated_facts(source);
CREATE INDEX IF NOT EXISTS idx_consolidated_fact_sources_fact ON consolidated_fact_sources(fact_id);
CREATE INDEX IF NOT EXISTS idx_consolidated_fact_sources_event ON consolidated_fact_sources(event_id);
CREATE INDEX IF NOT EXISTS idx_experience_nodes_session_time ON experience_nodes(session_id, created_at);
