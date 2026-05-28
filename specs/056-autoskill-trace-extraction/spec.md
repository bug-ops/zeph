---
aliases:
  - AutoSkill A1
  - Conversation Trace Skill Extraction
  - Trace-to-Skill Pipeline
tags:
  - sdd
  - spec
  - skills
  - self-learning
  - autoskill
created: 2026-05-19
status: draft
related:
  - "[[005-skills/spec]]"
  - "[[015-self-learning/spec]]"
  - "[[001-system-invariants/spec]]"
  - "[[057-agent-persistence/spec]]"
---

# Spec: Conversation Trace → Skill Extraction Pipeline (AutoSkill A1)

> [!info]
> GitHub Issue: #4447
> Priority: P3
> Crate: `zeph-skills`, `zeph-agent-persistence`
>
> After each session completes, pass the user-turn history through an LLM extractor
> that proposes SKILL.md candidates. Candidates are saved as draft skills at
> `quarantined` trust level for user review. This is opt-in and runs fully asynchronously
> after session end — it NEVER blocks a live turn or writes automatically to the skill corpus.

## Overview

### Problem Statement

Zeph's existing self-learning pipeline (ARISE, STEM, ERL) is reactive: it extracts patterns
from failures, successful tool sequences, and per-turn heuristics. It does not mine
completed conversation sessions to discover new reusable skills from scratch. AutoSkill
demonstrates that raw conversation history contains rich latent skill signal: 1,858 skills
extracted from 10,243 conversations cover programming, writing, and data/AI-ML domains.

### Goal

Provide an opt-in, background pipeline that extracts skill candidates from completed
conversation traces (user messages only, excluding assistant responses) and deposits them
as quarantined drafts requiring explicit user approval before becoming part of the skill corpus.

### Out of Scope

- Automatic promotion of extracted candidates to `Provisional` or `Trusted` trust without user action
- Processing assistant messages (deliberate exclusion, mirroring AutoSkill design)
- Real-time extraction during a live turn
- Extraction from partial or interrupted sessions

---

## Key Invariants

- **Opt-in only**: extraction is disabled by default (`trace_extraction_enabled = false`).
  No session is ever processed without the user explicitly enabling this feature.
- **No automatic corpus writes**: extracted candidates are ALWAYS saved at `quarantined`
  trust level. They do not become active skills until the user explicitly promotes them.
- **User-turn messages only**: the LLM extractor receives only `role = "user"` messages
  from the session history. Assistant responses are deliberately excluded to focus the
  extractor on reusable capability patterns, not on memorizing specific answers.
- **Async post-session only**: extraction runs in a `tokio::spawn` background task, fired
  after the session's final message is persisted. It MUST NOT run during a live turn.
- **Deduplication before write**: every candidate is checked against the existing registry
  via cosine similarity (`dedup_threshold`, default 0.90). Near-duplicates enter the
  versioned merge flow (spec 057-autoskill-versioned-merging) instead of creating new entries.
- **Trust governance always applies**: candidates enter the standard quarantine review flow
  (capability escalation check, injection scan, user approval). No bypass.
- **Provider must be configurable**: LLM extraction calls MUST use a named provider from
  `[[llm.providers]]`, defaulting to the primary provider if `trace_extraction_provider`
  is empty. NEVER hardcode a model name.
- **Session-level idempotency**: a processed session MUST be marked in `skill_trace_sessions`
  to prevent re-extraction on restart or config reload.

---

## Requirements

### Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `skills.learning.trace_extraction_enabled = true` AND a session ends, THE SYSTEM SHALL spawn a background task to extract skill candidates from that session's user-turn history | must |
| FR-002 | WHEN extracting, THE SYSTEM SHALL send ONLY user-role messages (no assistant messages) to the extraction LLM | must |
| FR-003 | WHEN the LLM returns skill candidates, THE SYSTEM SHALL run cosine similarity dedup against the current registry; similarity ≥ `dedup_threshold` (default 0.90) routes the candidate to the versioned merge flow (see spec 057); similarity < `dedup_threshold` creates a new quarantined draft | must |
| FR-004 | THE SYSTEM SHALL save each novel candidate as a SKILL.md at `quarantined` trust level in the managed skills directory | must |
| FR-005 | THE SYSTEM SHALL record each processed session in `skill_trace_sessions` SQLite table to prevent re-extraction | must |
| FR-006 | WHEN extraction is running, THE SYSTEM SHALL display a TUI status indicator (`Extracting skills from session…`) | must |
| FR-007 | THE SYSTEM SHALL expose a CLI subcommand `zeph skills extract <session_id>` to manually trigger extraction for a specific session | should |
| FR-008 | WHEN `trace_extraction_max_sessions_queued` is exceeded, THE SYSTEM SHALL drop the oldest pending extraction task and emit a debug log | should |
| FR-009 | THE SYSTEM SHALL log the count of candidates proposed, candidates deduped, candidates saved, and candidates routed to merge per session | must |

### Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Latency | Extraction MUST NOT add latency to the active agent turn — runs post-session only |
| NFR-002 | Reliability | Extraction failure MUST be logged and silently dropped — NEVER crash the agent process |
| NFR-003 | Resource | A single extraction task MUST NOT process more than `trace_extraction_max_turns` (default 200) user messages to bound LLM token cost |
| NFR-004 | Security | Extracted SKILL.md candidates MUST pass the injection sanitization scan before being written to disk |
| NFR-005 | Observability | Tracing spans under `skills.trace_extraction.*` for LLM call, dedup check, and write |

---

## SKILL.md Schema Changes

Extracted candidates are standard SKILL.md files. No new frontmatter fields are required
specifically for A1. The `source` metadata field (existing) is set to `trace_extraction`.

```yaml
---
name: <extracted-name>
description: <extracted-description>
version: 0
source: trace_extraction
session_id: <originating-session-id>
---
```

The `version` field is specified by spec 057 (AutoSkill A2). It must be present with
value `0` on all newly extracted candidates so the versioned merge flow can handle them.

---

## Config Fields

All fields live under `[skills.learning]`:

```toml
[skills.learning]
# A1: Conversation trace extraction
trace_extraction_enabled = false               # opt-in; default off
trace_extraction_provider = ""                 # named [[llm.providers]] name; empty = primary
trace_extraction_embed_provider = ""           # embed provider for dedup; empty = primary embed provider
trace_extraction_max_turns = 200               # max user messages sent per session
trace_extraction_max_sessions_queued = 10      # max concurrent background extraction tasks
```

- `trace_extraction_provider`: resolves via `ProviderRegistry::get_by_name()`. Empty string
  falls back to the default provider. Unknown name emits a warning and falls back — never panics.
- `trace_extraction_embed_provider`: embed provider for dedup; empty = primary embed provider.
  Resolves via `ProviderRegistry::get_by_name()`. Empty string falls back to the default provider.
  Unknown name emits a warning and falls back — never panics.

---

## Database Changes

New table `skill_trace_sessions`:

```sql
CREATE TABLE skill_trace_sessions (
    session_id     TEXT    NOT NULL PRIMARY KEY,
    processed_at   INTEGER NOT NULL,   -- Unix timestamp
    candidates_proposed  INTEGER NOT NULL DEFAULT 0,
    candidates_saved     INTEGER NOT NULL DEFAULT 0,
    candidates_merged    INTEGER NOT NULL DEFAULT 0
);
```

Migration number: assign the next available migration in `crates/zeph-db/migrations/`.

---

## Acceptance Criteria

```
GIVEN trace_extraction_enabled = false
WHEN a session ends
THEN no extraction task is spawned and no skill_trace_sessions row is written

GIVEN trace_extraction_enabled = true AND a session with 5 user turns ends
WHEN extraction completes
THEN a skill_trace_sessions row exists for that session_id
AND extracted candidates are present as quarantined SKILL.md files
AND no turn latency was added during the session

GIVEN a session was already extracted (row exists in skill_trace_sessions)
WHEN the agent restarts and the same session_id is encountered
THEN the extraction task is NOT re-run

GIVEN an extracted candidate has cosine similarity >= dedup_threshold to an existing skill
WHEN extraction processes that candidate
THEN the candidate is NOT written as a new SKILL.md
AND it is routed to the versioned merge flow (spec 057)

GIVEN trace_extraction_provider = "fast" (valid named provider)
WHEN extraction runs
THEN the LLM call uses the "fast" provider

GIVEN the extraction LLM call fails
WHEN extraction encounters the error
THEN the error is logged at WARN level and the session is NOT marked as processed
```

---

## NEVER

- NEVER write a skill candidate directly at `Provisional` or `Trusted` trust — always `quarantined`
- NEVER include assistant-role messages in the extraction LLM prompt
- NEVER run extraction synchronously during a live agent turn
- NEVER re-extract a session that already has a `skill_trace_sessions` row
- NEVER bypass injection sanitization before writing a candidate to disk
- NEVER hardcode a model name for the extraction provider — always resolve via `[[llm.providers]]`
- NEVER skip `trace_extraction_max_turns` truncation — uncapped sessions can exhaust token budgets

---

## Agent Boundaries

### Always (without asking)
- Save extracted candidates at `quarantined` trust level
- Run injection scan before writing any candidate
- Mark processed sessions in `skill_trace_sessions`
- Use `trace_extraction_provider` resolved via `ProviderRegistry`

### Ask First
- Changing the user-messages-only policy (involves trust model implications)
- Increasing `trace_extraction_max_turns` default above 200
- Adding assistant messages to the extraction context

### Never
- Auto-promote extracted skills without user approval
- Run extraction during a live turn
- Process sessions without the opt-in flag enabled

---

## See Also

- [[005-skills/spec]] — SKILL.md format, registry, trust governance
- [[015-self-learning/spec]] — ARISE, STEM, ERL, trust model
- [[057-autoskill-versioned-merging/spec]] — versioned merge flow for near-duplicates
- [[001-system-invariants/spec]] — system contracts
