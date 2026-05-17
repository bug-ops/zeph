-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Experience nodes: records of tool execution outcomes in the agent loop
CREATE TABLE IF NOT EXISTS experience_nodes (
    id          BIGSERIAL PRIMARY KEY,
    session_id  TEXT    NOT NULL,
    turn        BIGINT  NOT NULL,
    tool_name   TEXT    NOT NULL,
    outcome     TEXT    NOT NULL,
    detail      TEXT,
    error_ctx   TEXT,
    created_at  BIGINT  NOT NULL DEFAULT EXTRACT(EPOCH FROM NOW())::BIGINT
);

-- Experience edges: temporal sequence between consecutive experience nodes
CREATE TABLE IF NOT EXISTS experience_edges (
    id              BIGSERIAL PRIMARY KEY,
    source_exp_id   BIGINT NOT NULL REFERENCES experience_nodes(id),
    target_exp_id   BIGINT NOT NULL REFERENCES experience_nodes(id),
    relation        TEXT   NOT NULL DEFAULT 'followed_by'
);

-- Links between experience nodes and knowledge graph entities
CREATE TABLE IF NOT EXISTS experience_entity_links (
    experience_id   BIGINT NOT NULL REFERENCES experience_nodes(id),
    entity_id       BIGINT NOT NULL REFERENCES graph_entities(id),
    PRIMARY KEY (experience_id, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_experience_nodes_session      ON experience_nodes(session_id, turn);
CREATE INDEX IF NOT EXISTS idx_experience_nodes_tool         ON experience_nodes(tool_name);
CREATE INDEX IF NOT EXISTS idx_experience_entity_links       ON experience_entity_links(entity_id);
-- Facilitates session-time lookups used by episodic_consolidation
CREATE INDEX IF NOT EXISTS idx_experience_nodes_session_time ON experience_nodes(session_id, created_at);
