-- Make first_message_id and last_message_id nullable in summaries table.
-- PostgreSQL supports ALTER COLUMN DROP NOT NULL directly.

ALTER TABLE summaries ALTER COLUMN first_message_id DROP NOT NULL;
ALTER TABLE summaries ALTER COLUMN last_message_id DROP NOT NULL;
