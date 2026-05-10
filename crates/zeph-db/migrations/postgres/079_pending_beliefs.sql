-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- TODO: Port pending_beliefs + belief_evidence tables from SQLite migration 084 to PostgreSQL.
-- Differences to address:
--   - REFERENCES graph_entities(id) / graph_edges(id): add ON DELETE CASCADE if desired
--   - unixepoch() → EXTRACT(EPOCH FROM NOW())::BIGINT
--   - Partial indexes: supported in PostgreSQL, no changes needed
--   - prob CHECK constraint: supported in PostgreSQL unchanged
