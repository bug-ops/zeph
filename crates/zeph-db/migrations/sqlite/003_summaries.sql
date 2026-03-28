CREATE TABLE IF NOT EXISTS summaries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    content TEXT NOT NULL,
    first_message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    last_message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    token_estimate INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_summaries_conversation
    ON summaries(conversation_id);
