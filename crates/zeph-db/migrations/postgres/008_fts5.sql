-- PostgreSQL equivalent of SQLite FTS5 for messages.
-- Uses a tsvector column with GIN index and trigger for auto-sync.

ALTER TABLE messages ADD COLUMN tsv tsvector;

CREATE INDEX idx_messages_fts ON messages USING GIN(tsv);

CREATE OR REPLACE FUNCTION messages_tsv_update() RETURNS trigger AS $$
BEGIN
    NEW.tsv := to_tsvector('english', COALESCE(NEW.content, ''));
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER messages_tsv_trigger
    BEFORE INSERT OR UPDATE OF content ON messages
    FOR EACH ROW EXECUTE FUNCTION messages_tsv_update();

-- Backfill existing rows.
UPDATE messages SET tsv = to_tsvector('english', COALESCE(content, ''));
