-- Records how the body of a stored tool result was classified when it was saved, so a later
-- retrieval can restore the same classification (spec 040).
-- result_source: 1 native local, 2 native web, 3 native memory, 4 MCP-served (mcp_server_id names
-- the server when known), 5 archived local-untrusted text, 6 archived external-untrusted text.
-- NULL means the row was recorded before this column existed; readers treat it as MCP-served.
ALTER TABLE tool_overflow ADD COLUMN result_source SMALLINT CHECK (result_source IN (1, 2, 3, 4, 5, 6));
ALTER TABLE tool_overflow ADD COLUMN mcp_server_id TEXT;
