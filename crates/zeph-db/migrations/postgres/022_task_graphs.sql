-- Task graph persistence (Phase 1).
-- The full TaskGraph is stored as a JSON blob in `graph_json`.
CREATE TABLE IF NOT EXISTS task_graphs (
    id          TEXT PRIMARY KEY,
    goal        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'created',
    graph_json  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_task_graphs_status  ON task_graphs(status);
CREATE INDEX IF NOT EXISTS idx_task_graphs_created ON task_graphs(created_at);
