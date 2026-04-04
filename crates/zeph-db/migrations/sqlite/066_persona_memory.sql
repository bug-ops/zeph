-- Persona memory layer (#2461).
-- Stores long-lived user attributes (preferences, domain knowledge, working style)
-- extracted from conversation history via LLM.
CREATE TABLE IF NOT EXISTS persona_memory (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Category of persona fact for structured retrieval
    category TEXT NOT NULL CHECK(category IN (
        'preference', 'domain_knowledge', 'working_style',
        'communication', 'background'
    )),
    -- The extracted fact content
    content TEXT NOT NULL,
    -- Confidence score from extraction (0.0 - 1.0)
    confidence REAL NOT NULL DEFAULT 0.5,
    -- How many times this fact was reinforced across sessions
    evidence_count INTEGER NOT NULL DEFAULT 1,
    -- Source conversation ID where first extracted (provenance only, no FK)
    source_conversation_id INTEGER,
    -- Nullable FK to persona_memory.id: when set, this fact supersedes an older one
    supersedes_id INTEGER REFERENCES persona_memory(id) ON DELETE SET NULL,
    -- Timestamps
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_persona_memory_category ON persona_memory(category);
CREATE INDEX IF NOT EXISTS idx_persona_memory_confidence ON persona_memory(confidence DESC);
CREATE INDEX IF NOT EXISTS idx_persona_memory_supersedes ON persona_memory(supersedes_id)
    WHERE supersedes_id IS NOT NULL;

-- Dedup index: prevent exact duplicate persona facts within a category.
-- The upsert increments evidence_count and updates confidence on conflict.
CREATE UNIQUE INDEX IF NOT EXISTS idx_persona_memory_uniq
    ON persona_memory(category, content);

-- Per-session extraction tracking: avoids re-processing already-extracted messages.
CREATE TABLE IF NOT EXISTS persona_meta (
    id INTEGER PRIMARY KEY CHECK(id = 1),  -- singleton row
    last_extracted_message_id INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT OR IGNORE INTO persona_meta(id, last_extracted_message_id) VALUES(1, 0);
