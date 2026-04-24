-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Experience nodes: records of tool execution outcomes in the agent loop
CREATE TABLE experience_nodes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT    NOT NULL,
    turn        INTEGER NOT NULL,
    tool_name   TEXT    NOT NULL,
    outcome     TEXT    NOT NULL,
    detail      TEXT,
    error_ctx   TEXT,
    created_at  INTEGER NOT NULL
);

-- Experience edges: temporal sequence between consecutive experience nodes
CREATE TABLE experience_edges (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_exp_id   INTEGER NOT NULL REFERENCES experience_nodes(id),
    target_exp_id   INTEGER NOT NULL REFERENCES experience_nodes(id),
    relation        TEXT    NOT NULL DEFAULT 'followed_by'
);

-- Links between experience nodes and knowledge graph entities
CREATE TABLE experience_entity_links (
    experience_id   INTEGER NOT NULL REFERENCES experience_nodes(id),
    entity_id       INTEGER NOT NULL REFERENCES graph_entities(id),
    PRIMARY KEY (experience_id, entity_id)
);

CREATE INDEX idx_experience_nodes_session ON experience_nodes(session_id, turn);
CREATE INDEX idx_experience_nodes_tool    ON experience_nodes(tool_name);
CREATE INDEX idx_experience_entity_links  ON experience_entity_links(entity_id);
