-- PostgreSQL equivalent of SQLite FTS5 for graph entities.
-- Uses tsvector with weighted columns: name (A=high), summary (B=medium).

ALTER TABLE graph_entities ADD COLUMN tsv tsvector;

CREATE INDEX idx_graph_entities_fts ON graph_entities USING GIN(tsv);

CREATE OR REPLACE FUNCTION graph_entities_tsv_update() RETURNS trigger AS $$
BEGIN
    NEW.tsv :=
        setweight(to_tsvector('english', COALESCE(NEW.name, '')), 'A') ||
        setweight(to_tsvector('english', COALESCE(NEW.summary, '')), 'B');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER graph_entities_tsv_trigger
    BEFORE INSERT OR UPDATE OF name, summary ON graph_entities
    FOR EACH ROW EXECUTE FUNCTION graph_entities_tsv_update();

-- Backfill existing entities.
UPDATE graph_entities SET tsv =
    setweight(to_tsvector('english', COALESCE(name, '')), 'A') ||
    setweight(to_tsvector('english', COALESCE(summary, '')), 'B');
