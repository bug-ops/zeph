-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Kumiho belief revision: audit trail pointer linking old edge to its replacement.
-- superseded_by IS NULL for active edges; set to new_edge_id when invalidated via belief revision.
ALTER TABLE graph_edges ADD COLUMN IF NOT EXISTS superseded_by INTEGER REFERENCES graph_edges(id) ON DELETE SET NULL;

-- Index to efficiently query "what was superseded by edge X?" (reverse traversal of revision chain).
CREATE INDEX IF NOT EXISTS idx_graph_edges_superseded_by ON graph_edges(superseded_by)
    WHERE superseded_by IS NOT NULL;
