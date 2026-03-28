-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Remove any existing self-loop edges.
DELETE FROM graph_edges WHERE source_entity_id = target_entity_id;

-- Prevent future self-loop edges at the DB level (SQLite has no CHECK on ALTER TABLE).
CREATE TRIGGER IF NOT EXISTS graph_edges_no_self_loops
    BEFORE INSERT ON graph_edges
BEGIN
    SELECT RAISE(ABORT, 'self-loop edge rejected: source and target entity must differ')
    WHERE NEW.source_entity_id = NEW.target_entity_id;
END;
