-- CAM Phase 2-B: persist fidelity level per message (#4550).
-- 0 = Full (default), 1 = Compressed, 2 = Placeholder.
-- DEFAULT 0 ensures existing rows and rows from disabled-CAM paths
-- are treated as Full without application-level intervention.
ALTER TABLE messages ADD COLUMN fidelity_tag INTEGER NOT NULL DEFAULT 0;
