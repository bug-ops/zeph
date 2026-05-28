-- RTW-A re-entry defense: track the provenance of each scheduled job.
-- Existing rows default to 'external' (the most restrictive level) so they are
-- subject to the full quarantine and injection-detection pipeline on the first run
-- after upgrade — a correct fail-safe default.
ALTER TABLE scheduled_jobs ADD COLUMN IF NOT EXISTS provenance TEXT NOT NULL DEFAULT 'external';
