-- Migration 024: Entity canonicalization with alias table.
-- PostgreSQL supports ALTER TABLE directly — no table recreation needed.

-- 1. Add canonical_name column (nullable, then backfill, then constrain).
ALTER TABLE graph_entities ADD COLUMN IF NOT EXISTS canonical_name TEXT;
UPDATE graph_entities SET canonical_name = name WHERE canonical_name IS NULL;
ALTER TABLE graph_entities ALTER COLUMN canonical_name SET NOT NULL;

-- 2. Drop the old UNIQUE(name, entity_type) constraint and add UNIQUE(canonical_name, entity_type).
ALTER TABLE graph_entities DROP CONSTRAINT IF EXISTS graph_entities_name_entity_type_key;
ALTER TABLE graph_entities ADD CONSTRAINT graph_entities_canonical_name_entity_type_key
    UNIQUE (canonical_name, entity_type);

-- 3. Rebuild indexes with LOWER() for case-insensitive lookups (no citext dependency).
DROP INDEX IF EXISTS idx_graph_entities_name;
CREATE INDEX IF NOT EXISTS idx_graph_entities_name ON graph_entities(LOWER(name));
CREATE INDEX IF NOT EXISTS idx_graph_entities_canonical ON graph_entities(LOWER(canonical_name));

-- idx_graph_entities_type and idx_graph_entities_last_seen already exist from migration 021.

-- 4. Update tsvector trigger to include canonical_name (migration 023 trigger is updated here).
CREATE OR REPLACE FUNCTION graph_entities_tsv_update() RETURNS trigger AS $$
BEGIN
    NEW.tsv :=
        setweight(to_tsvector('english', COALESCE(NEW.name, '')), 'A') ||
        setweight(to_tsvector('english', COALESCE(NEW.canonical_name, '')), 'A') ||
        setweight(to_tsvector('english', COALESCE(NEW.summary, '')), 'B');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Backfill tsvector for existing rows after canonical_name is populated.
UPDATE graph_entities SET tsv =
    setweight(to_tsvector('english', COALESCE(name, '')), 'A') ||
    setweight(to_tsvector('english', COALESCE(canonical_name, '')), 'A') ||
    setweight(to_tsvector('english', COALESCE(summary, '')), 'B');

-- 5. Alias table: maps variant surface forms to canonical entity IDs.
CREATE TABLE IF NOT EXISTS graph_entity_aliases (
    id          BIGSERIAL PRIMARY KEY,
    entity_id   BIGINT NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    alias_name  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(alias_name, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_entity_aliases_name
    ON graph_entity_aliases(LOWER(alias_name));
CREATE INDEX IF NOT EXISTS idx_graph_entity_aliases_entity
    ON graph_entity_aliases(entity_id);

-- 6. Seed initial aliases from existing entity names.
INSERT INTO graph_entity_aliases (entity_id, alias_name)
SELECT id, name FROM graph_entities
ON CONFLICT DO NOTHING;
