-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Add chunk_index to embeddings_metadata to support multi-vector chunked embeddings.
ALTER TABLE embeddings_metadata ADD COLUMN IF NOT EXISTS chunk_index INTEGER NOT NULL DEFAULT 0;

ALTER TABLE embeddings_metadata DROP CONSTRAINT IF EXISTS embeddings_metadata_message_id_model_key;
ALTER TABLE embeddings_metadata ADD CONSTRAINT embeddings_metadata_message_chunk_model_key
    UNIQUE (message_id, chunk_index, model);

CREATE INDEX IF NOT EXISTS idx_embeddings_metadata_message_id
    ON embeddings_metadata(message_id);
