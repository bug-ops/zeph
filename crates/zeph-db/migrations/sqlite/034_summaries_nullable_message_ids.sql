-- Make first_message_id and last_message_id nullable in summaries table.
-- Session-level summaries (e.g. shutdown summaries) do not correspond to a specific message
-- range and should be able to store NULL in these columns.
-- SQLite does not support ALTER COLUMN, so we use CREATE+INSERT+DROP+RENAME.

CREATE TABLE IF NOT EXISTS summaries_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    content TEXT NOT NULL,
    first_message_id INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    last_message_id INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    token_estimate INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO summaries_new
    (id, conversation_id, content, first_message_id, last_message_id, token_estimate, created_at)
SELECT id, conversation_id, content, first_message_id, last_message_id, token_estimate, created_at
FROM summaries;

DROP TABLE summaries;

ALTER TABLE summaries_new RENAME TO summaries;

CREATE INDEX IF NOT EXISTS idx_summaries_conversation
    ON summaries(conversation_id);
