-- Postgres migration 103; SQLite counterpart is 102. Parity guard matches on logical name 'graph_provenance', not number.
-- Knowledge-ingest Phase 0 (#5015): provenance columns on graph tables so recall can
-- isolate imported knowledge from conversation-derived knowledge (spec-067 INV-2/INV-3).
-- DEFAULT 'conversation' backfills every existing row to the conversation origin.
ALTER TABLE graph_edges    ADD COLUMN IF NOT EXISTS origin          TEXT NOT NULL DEFAULT 'conversation';
ALTER TABLE graph_edges    ADD COLUMN IF NOT EXISTS import_batch_id TEXT;
ALTER TABLE graph_edges    ADD COLUMN IF NOT EXISTS source_uri      TEXT;
ALTER TABLE graph_entities ADD COLUMN IF NOT EXISTS origin          TEXT NOT NULL DEFAULT 'conversation';
ALTER TABLE graph_entities ADD COLUMN IF NOT EXISTS import_batch_id TEXT;
