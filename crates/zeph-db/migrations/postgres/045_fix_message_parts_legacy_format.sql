-- Rewrite legacy externally-tagged MessagePart JSON in the messages table.
-- Old format (pre-v0.17.1): [{"Summary":{"text":"..."}}, ...]
-- New format (v0.17.1+):    [{"kind":"summary","text":"..."}, ...]
-- Detection: presence of known variant name as an object key.
-- Rows with complex old-format parts are reset to parts='[]'.
UPDATE messages
SET parts = '[]'
WHERE parts::text != '[]'
  AND (
    parts::text LIKE '%{"Text":%'
    OR parts::text LIKE '%{"ToolOutput":%'
    OR parts::text LIKE '%{"Recall":%'
    OR parts::text LIKE '%{"CodeContext":%'
    OR parts::text LIKE '%{"Summary":%'
    OR parts::text LIKE '%{"CrossSession":%'
    OR parts::text LIKE '%{"ToolUse":%'
    OR parts::text LIKE '%{"ToolResult":%'
    OR parts::text LIKE '%{"Image":%'
    OR parts::text LIKE '%{"ThinkingBlock":%'
    OR parts::text LIKE '%{"RedactedThinkingBlock":%'
    OR parts::text LIKE '%{"Compaction":%'
  );
