-- FTS5 virtual table for keyword search over messages.
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    content='messages',
    content_rowid='id'
);

-- Backfill existing messages into the FTS index.
INSERT INTO messages_fts(rowid, content)
    SELECT id, content FROM messages;

-- Keep FTS index in sync on INSERT.
CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages
BEGIN
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;

-- Keep FTS index in sync on DELETE.
CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages
BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;

-- Keep FTS index in sync on UPDATE.
CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages
BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;
