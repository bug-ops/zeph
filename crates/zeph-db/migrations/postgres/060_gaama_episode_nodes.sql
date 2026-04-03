-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- GAAMA episode nodes: one episode per conversation.
CREATE TABLE IF NOT EXISTS graph_episodes (
    id BIGSERIAL PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at TIMESTAMPTZ,
    UNIQUE(conversation_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_episodes_conv ON graph_episodes(conversation_id);

-- Entity-episode membership.
CREATE TABLE IF NOT EXISTS graph_episode_entities (
    episode_id BIGINT NOT NULL REFERENCES graph_episodes(id) ON DELETE CASCADE,
    entity_id BIGINT NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (episode_id, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_episode_entities_entity
    ON graph_episode_entities(entity_id);

-- Rename graph_edges.episode_id to source_message_id.
ALTER TABLE graph_edges ADD COLUMN IF NOT EXISTS source_message_id BIGINT REFERENCES messages(id) ON DELETE SET NULL;
UPDATE graph_edges SET source_message_id = episode_id WHERE episode_id IS NOT NULL;
