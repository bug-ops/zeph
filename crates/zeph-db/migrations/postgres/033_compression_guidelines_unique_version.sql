-- Add UNIQUE(version) constraint to compression_guidelines.
-- PostgreSQL supports this directly without table recreation.
-- First deduplicate: keep only the row with the highest id per version.

DELETE FROM compression_guidelines
WHERE id NOT IN (
    SELECT MAX(id) FROM compression_guidelines GROUP BY version
);

ALTER TABLE compression_guidelines ADD CONSTRAINT compression_guidelines_version_key UNIQUE (version);
