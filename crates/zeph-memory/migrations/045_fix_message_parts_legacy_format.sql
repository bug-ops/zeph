-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Rewrite legacy externally-tagged MessagePart JSON in the messages table.
-- Old format (pre-v0.17.1): [{"Summary":{"text":"..."}}, ...]
-- New format (v0.17.1+):    [{"kind":"summary","text":"..."}, ...]
-- Detection: presence of known variant name as an object key.
-- SQLite lacks procedural JSON rewriting; rows with complex old-format parts
-- are reset to parts='[]'. The plain-text content column always holds the
-- message text and is used as the display fallback.
UPDATE messages
SET parts = '[]'
WHERE parts != '[]'
  AND (
    parts LIKE '%{"Text":%'
    OR parts LIKE '%{"ToolOutput":%'
    OR parts LIKE '%{"Recall":%'
    OR parts LIKE '%{"CodeContext":%'
    OR parts LIKE '%{"Summary":%'
    OR parts LIKE '%{"CrossSession":%'
    OR parts LIKE '%{"ToolUse":%'
    OR parts LIKE '%{"ToolResult":%'
    OR parts LIKE '%{"Image":%'
    OR parts LIKE '%{"ThinkingBlock":%'
    OR parts LIKE '%{"RedactedThinkingBlock":%'
    OR parts LIKE '%{"Compaction":%'
  );
