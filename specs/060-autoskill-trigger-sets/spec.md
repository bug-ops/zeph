---
aliases:
  - AutoSkill A5
  - Trigger Set Indexing
  - SKILL.md Triggers
tags:
  - sdd
  - spec
  - skills
  - retrieval
  - autoskill
created: 2026-05-19
status: implemented
related:
  - "[[005-skills/spec]]"
  - "[[015-self-learning/spec]]"
  - "[[001-system-invariants/spec]]"
---

# Spec: Trigger Set Indexing in SKILL.md Frontmatter (AutoSkill A5)

> [!info]
> GitHub Issue: #4451
> Priority: P4
> Crate: `zeph-skills`
>
> Add an optional `triggers` list to SKILL.md frontmatter containing example queries that
> should activate the skill. At matcher construction, embed each trigger and store the
> vectors alongside the skill's description vector. At retrieval time, the maximum
> similarity across all trigger embeddings is included in the per-skill score.

## Overview

### Problem Statement

Skill descriptions are written to be concise and general. When a user query is phrased
in a concrete, example-specific way, it may have low cosine similarity to a general
description even though the underlying need is an exact match. AutoSkill's `trigger_set`
field addresses this by providing concrete example queries that are indexed as additional
retrieval vectors for the same skill.

Zeph's SKILL.md format already has a `triggers` field defined by the agentskills.io
specification (used for keyword fallback). This spec extends it to be embedded as full
semantic vectors in addition to keyword matching.

### Goal

At `SkillMatcher` construction, embed each `triggers` list entry and store the resulting
vectors associated with the skill. At retrieval, compute cosine similarity between the
query embedding and all trigger embeddings, and incorporate the max trigger similarity
into the final per-skill score using a configurable `trigger_weight`.

### Out of Scope

- Automatically generating trigger examples via LLM (that is a future UX enhancement)
- Storing trigger embeddings in Qdrant as separate points (in-memory only for v1)
- Keyword fallback using `triggers` (already implemented — this spec adds embedding only)

---

## Key Invariants

- **`triggers` is already a valid SKILL.md field**: this spec adds embedding-based indexing
  to an existing field. No breaking change to the SKILL.md format.
- **In-memory storage only for v1**: trigger embeddings are stored in `SkillMatcher` memory
  alongside description embeddings. They are NOT persisted to Qdrant as separate points.
  This means they are recomputed on every matcher construction (at startup and hot-reload).
- **Score aggregation is MAX, not average**: the per-skill trigger score is `max(cosine(query, trigger_i) for all triggers)`. Average would dilute the signal with irrelevant triggers.
- **`trigger_weight` controls blending**: final skill score combines description and trigger
  signals:
  ```
  trigger_score = max(cosine(query, trigger_i))   // 0.0 if no triggers
  description_score = cosine(query, description_embedding)
  combined = (1 - trigger_weight) * description_score + trigger_weight * trigger_score
  ```
  When a skill has no triggers, `trigger_score = 0.0` and `trigger_weight` effectively
  reduces the weight on the description score. To avoid penalizing skills without triggers,
  the formula uses `trigger_weight` only when `triggers` is non-empty:
  ```
  if triggers.is_empty() { combined = description_score }
  ```
- **Embedding provider is the same as for descriptions**: trigger embeddings use the
  `embedding_provider` (resolved once at bootstrap). No new provider field.
- **Hot-reload recomputes trigger embeddings**: when a SKILL.md is reloaded, all trigger
  embeddings for that skill are recomputed.
- **`max_triggers_per_skill`**: to bound memory and startup latency, only the first
  `max_triggers_per_skill` (default 10) triggers are embedded. Additional triggers are
  silently ignored.

---

## Requirements

### Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `SkillMatcher` is constructed, THE SYSTEM SHALL embed each `triggers` entry for each skill using the `embedding_provider` | must |
| FR-002 | WHEN computing a skill's retrieval score, THE SYSTEM SHALL incorporate the max cosine similarity across all trigger embeddings using the formula described above | must |
| FR-003 | WHEN a skill has no `triggers` entries, THE SYSTEM SHALL use description similarity only (no penalty) | must |
| FR-004 | THE SYSTEM SHALL limit embedded triggers per skill to `max_triggers_per_skill` (default 10); excess entries are silently dropped | must |
| FR-005 | WHEN a SKILL.md is hot-reloaded, THE SYSTEM SHALL recompute trigger embeddings for that skill | must |
| FR-006 | WHEN `trigger_weight = 0.0`, THE SYSTEM SHALL skip trigger embedding computation entirely for performance | should |
| FR-007 | THE SYSTEM SHALL log trigger embedding counts per skill at DEBUG level on construction | should |

### Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Memory | Trigger embeddings are stored in-memory; with 500 skills × 10 triggers × 1536 floats × 4 bytes = ~30MB maximum. This is acceptable. |
| NFR-002 | Startup Latency | Trigger embedding calls at startup are batched where the embedding provider supports batch calls |
| NFR-003 | Correctness | Skills without `triggers` MUST score identically to the current system when `trigger_weight = 0.0` |

---

## SKILL.md Schema Changes

The `triggers` field already exists in the agentskills.io specification. No new field is
introduced. Authors can populate it with example queries:

```yaml
---
name: professional-rewrite
description: Rewrite text in a professional and formal tone
triggers:
  - "make this sound more professional"
  - "rewrite this email formally"
  - "polish this text for business context"
version: 0
---
```

Existing skills without `triggers` continue to work without modification.

---

## Config Fields

All fields live under `[skills]`:

```toml
[skills]
trigger_weight = 0.3              # NEW: blend weight for trigger similarity [0.0, 1.0]
                                  # 0.0 disables trigger embedding entirely
max_triggers_per_skill = 10       # NEW: max triggers embedded per skill
```

- `trigger_weight = 0.0` disables the feature entirely. Description-only scoring is used.
- `trigger_weight` range: `[0.0, 1.0]`. Values outside this range are clamped at startup
  with a WARN log.

---

## Acceptance Criteria

```
GIVEN a skill with triggers: ["make this more formal", "professional tone please"]
AND a user query "please make my email sound more professional"
WHEN skill matching runs with trigger_weight = 0.3
THEN the skill's score incorporates the max cosine similarity from trigger embeddings
AND the skill ranks higher than it would with description similarity alone

GIVEN a skill with no triggers field
WHEN skill matching runs with trigger_weight = 0.3
THEN the skill's score equals description cosine similarity (no penalty)

GIVEN trigger_weight = 0.0
WHEN SkillMatcher is constructed
THEN no trigger embeddings are computed (no embedding provider calls for triggers)

GIVEN a SKILL.md with 15 trigger entries AND max_triggers_per_skill = 10
WHEN SkillMatcher is constructed
THEN only the first 10 triggers are embedded; entries 11-15 are silently ignored

GIVEN a SKILL.md is hot-reloaded with updated triggers
WHEN the reload event fires
THEN the trigger embeddings for that skill are recomputed
```

---

## NEVER

- NEVER penalize a skill for having no triggers — description-only scoring is the baseline
- NEVER use average cosine across triggers — always use max
- NEVER use a different embedding provider for triggers vs. descriptions
- NEVER persist trigger embeddings to Qdrant in v1 — in-memory only
- NEVER embed more than `max_triggers_per_skill` triggers per skill

---

## Agent Boundaries

### Always (without asking)
- Skip trigger embedding when `trigger_weight = 0.0`
- Use max (not average) across trigger similarity scores
- Recompute trigger embeddings on hot-reload

### Ask First
- Persisting trigger embeddings to Qdrant (requires schema change)
- Auto-generating trigger examples via LLM

### Never
- Use a separate embedding provider for triggers
- Store triggers as separate Qdrant points in v1

---

## Implementation Notes

- Implemented in commit #4506 (A3+A4+A5 combined PR)

## See Also

- [[005-skills/spec]] — SKILL.md format, `SkillMatcher`, hybrid search
- [[059-autoskill-bm25-hybrid/spec]] — BM25 wiring (A4, complementary)
- [[058-autoskill-query-rewriting/spec]] — query rewriting (A3, complementary)
- [[001-system-invariants/spec]] — system contracts
