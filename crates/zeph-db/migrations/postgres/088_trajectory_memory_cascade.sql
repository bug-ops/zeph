-- Migration 088: Add ON DELETE CASCADE to trajectory_memory.conversation_id FK.
-- PostgreSQL cannot alter FK constraints in place; must drop and re-add.
ALTER TABLE trajectory_memory
    DROP CONSTRAINT IF EXISTS trajectory_memory_conversation_id_fkey,
    ADD CONSTRAINT trajectory_memory_conversation_id_fkey
        FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE;
