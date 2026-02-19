CREATE TABLE IF NOT EXISTS input_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    input TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
