-- Persona memory layer (#2461).
CREATE TABLE IF NOT EXISTS persona_memory (
    id                    BIGSERIAL PRIMARY KEY,
    category              TEXT NOT NULL CHECK(category IN (
                              'preference', 'domain_knowledge', 'working_style',
                              'communication', 'background'
                          )),
    content               TEXT NOT NULL,
    confidence            REAL NOT NULL DEFAULT 0.5,
    evidence_count        INTEGER NOT NULL DEFAULT 1,
    source_conversation_id BIGINT,
    supersedes_id         BIGINT REFERENCES persona_memory(id) ON DELETE SET NULL,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_persona_memory_category ON persona_memory(category);
CREATE INDEX IF NOT EXISTS idx_persona_memory_confidence ON persona_memory(confidence DESC);
CREATE INDEX IF NOT EXISTS idx_persona_memory_supersedes ON persona_memory(supersedes_id)
    WHERE supersedes_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_persona_memory_uniq
    ON persona_memory(category, content);

CREATE TABLE IF NOT EXISTS persona_meta (
    id                       INTEGER PRIMARY KEY CHECK(id = 1),
    last_extracted_message_id BIGINT NOT NULL DEFAULT 0,
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO persona_meta(id, last_extracted_message_id) VALUES(1, 0)
    ON CONFLICT (id) DO NOTHING;
