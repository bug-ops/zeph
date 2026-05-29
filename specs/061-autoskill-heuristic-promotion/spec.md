---
aliases:
  - AutoSkill A6
  - Heuristic Promotion
  - ERL to Full Skill Promotion
tags:
  - sdd
  - spec
  - skills
  - self-learning
  - autoskill
created: 2026-05-19
status: implemented
related:
  - "[[005-skills/spec]]"
  - "[[015-self-learning/spec]]"
  - "[[001-system-invariants/spec]]"
  - "[[057-autoskill-versioned-merging/spec]]"
---

# Spec: Periodic Heuristic Promotion from ERL to Full Skills (AutoSkill A6)

> [!info]
> GitHub Issue: #4452
> Priority: P4
> Crate: `zeph-skills`
>
> ERL (Experiential Reflective Learning) accumulates per-skill heuristics in `skill_heuristics`.
> When a skill accumulates enough unique heuristics, a periodic background job evaluates
> whether any heuristic cluster is substantial enough to become a standalone skill or to
> be merged into the parent skill body. Proposed promotions are saved as quarantined drafts
> requiring user review.

## Overview

### Problem Statement

ERL extracts transferable heuristics from successful skill+tool turns and stores them in
`skill_heuristics` with Jaccard deduplication. These heuristics are injected at matching
time as a `## Learned Heuristics` block. However, they exist only as runtime-injected text
and are never systematically evaluated for promotion into the permanent skill corpus.

This creates an asymmetry: valuable learned patterns accumulate in the heuristic store but
never graduate to become reusable standalone skills or enrich the parent skill's body.
AutoSkill's A6 closes this loop via periodic aggregation.

### Goal

Add a periodic (configurable interval) background job that scans `skill_heuristics` for
skills where the heuristic count exceeds `heuristic_promotion_threshold`. For qualifying
skills, an LLM evaluation call determines whether any heuristic subset is substantial
enough to:
1. Merge into the parent skill body (body enrichment), or
2. Become a standalone new skill (promotion to separate skill)

Both outcomes are saved as quarantined drafts. No automatic writes to active skills.

### Out of Scope

- Automatic merging of heuristics into active skill bodies without user review
- Real-time promotion (always periodic, never per-turn)
- Heuristic promotion from ERL entries below `erl_min_confidence`

---

## Key Invariants

- **Opt-in only**: `heuristic_promotion_enabled = false` by default. The periodic job is
  not started unless explicitly enabled.
- **Periodic, not per-turn**: promotion runs on a configurable interval
  (`heuristic_promotion_interval_hours`, default 24). It is NOT triggered by turn events.
- **No automatic active-skill writes**: promotion proposals are ALWAYS quarantined.
  They do not modify the currently active skill body. User must review and approve.
- **Only heuristics above `erl_min_confidence` are eligible**: promotion evaluation uses
  only heuristics that passed the ERL confidence gate at extraction time.
- **Heuristic count threshold**: promotion evaluation is triggered only when
  `COUNT(heuristics WHERE skill_name = X) >= heuristic_promotion_threshold` (default 5).
  Skills below this threshold are skipped.
- **LLM decides, human approves**: the LLM produces a promotion proposal. The human
  makes the final decision via the standard quarantine review flow.
- **Provider must be configurable**: promotion LLM calls use `heuristic_promotion_provider`
  from `[[llm.providers]]`. Empty = primary provider. A quality provider is appropriate
  here (this is an offline, non-latency-sensitive analysis). NEVER hardcode a model.
- **Already-proposed heuristics are not re-evaluated**: `skill_heuristic_promotions` table
  tracks which (skill_name, promotion_batch_hash) pairs have already been evaluated to
  prevent redundant LLM calls.

---

## Requirements

### Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `heuristic_promotion_enabled = true`, THE SYSTEM SHALL start a periodic background task at `heuristic_promotion_interval_hours` intervals | must |
| FR-002 | WHEN the periodic task runs, THE SYSTEM SHALL query `skill_heuristics` for skills with `COUNT >= heuristic_promotion_threshold` | must |
| FR-003 | WHEN a qualifying skill is found AND its heuristic batch hash differs from the last evaluated hash, THE SYSTEM SHALL call the LLM to evaluate whether any heuristics should be promoted | must |
| FR-004 | WHEN the LLM recommends body enrichment, THE SYSTEM SHALL create a quarantined draft of the parent skill with the heuristics integrated into its body | must |
| FR-005 | WHEN the LLM recommends a standalone skill, THE SYSTEM SHALL create a new quarantined SKILL.md candidate and route it through the Add/Merge/Discard flow (spec 057) | must |
| FR-006 | WHEN the LLM recommends no promotion (heuristics not substantial enough), THE SYSTEM SHALL record the evaluation in `skill_heuristic_promotions` and skip until heuristics change | must |
| FR-007 | THE SYSTEM SHALL display a TUI notification when promotion candidates are produced, listing the parent skill name and recommendation type | should |
| FR-008 | THE SYSTEM SHALL expose a CLI subcommand `zeph skills promote-heuristics [--skill <name>]` to trigger promotion evaluation manually | should |

### Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Isolation | Promotion job runs in a separate `tokio::spawn` task; failure MUST NOT affect the agent loop |
| NFR-002 | Idempotency | Running promotion twice on the same heuristic batch MUST produce the same result and NOT create duplicate quarantined drafts |
| NFR-003 | Observability | Tracing spans under `skills.heuristic_promotion.*` for scan, LLM call, and write |

---

## SKILL.md Schema Changes

Promotion-generated skills are standard SKILL.md files with `source = "heuristic_promotion"`.
They use the `version` field (spec 057) starting at `0`:

```yaml
---
name: <promoted-name>
description: <LLM-generated description>
version: 0
source: heuristic_promotion
parent_skill: <originating-skill-name>
---
```

The `parent_skill` field is a new optional SKILL.md frontmatter field introduced by this spec,
used for traceability only. It has no effect on matching or trust governance.

---

## Config Fields

All fields live under `[skills.learning]`:

```toml
[skills.learning]
# A6: Heuristic promotion
heuristic_promotion_enabled = false              # opt-in; default off
heuristic_promotion_provider = ""               # named [[llm.providers]] name; empty = primary
heuristic_promotion_threshold = 5               # min heuristic count to trigger evaluation
heuristic_promotion_interval_hours = 24         # evaluation interval
```

---

## Database Changes

New table `skill_heuristic_promotions`:

```sql
CREATE TABLE skill_heuristic_promotions (
    skill_name         TEXT    NOT NULL,
    batch_hash         TEXT    NOT NULL,   -- SHA-256 of sorted heuristic texts
    evaluated_at       INTEGER NOT NULL,   -- Unix timestamp
    recommendation     TEXT    NOT NULL,   -- "body_enrichment" | "new_skill" | "none"
    draft_skill_name   TEXT,               -- NULL if recommendation = "none"
    PRIMARY KEY (skill_name, batch_hash)
);
```

Migration number: **093** (`crates/zeph-db/migrations/093_skill_heuristic_promotions.sql`).

---

## LLM Promotion Prompt Contract

The promotion prompt receives:
1. The parent skill's current SKILL.md body
2. All qualifying heuristics (above `erl_min_confidence`, count ≥ threshold)
3. Instruction: evaluate whether the heuristics represent (a) improvements to the existing
   skill, (b) a distinct new capability, or (c) insufficient signal for either. Return one
   of: `body_enrichment <integrated_body>`, `new_skill <name> <body>`, `none`.

The response is parsed to determine recommendation type and extract the proposed content.
Parse failure is treated as `none`.

---

## Acceptance Criteria

```
GIVEN heuristic_promotion_enabled = false
WHEN the agent runs
THEN no promotion job is started and no skill_heuristic_promotions rows are written

GIVEN heuristic_promotion_enabled = true
AND skill "code-review" has 6 heuristics above erl_min_confidence
WHEN the promotion job runs
THEN the LLM is called with the skill body and 6 heuristics
AND the result is recorded in skill_heuristic_promotions

GIVEN the LLM recommends body_enrichment for "code-review"
WHEN the result is processed
THEN a quarantined draft of "code-review" with integrated heuristics is written
AND the existing "code-review" skill is NOT modified

GIVEN the promotion job runs and the heuristic batch for "code-review" is unchanged
WHEN the job runs a second time
THEN no LLM call is made (same batch hash in skill_heuristic_promotions)

GIVEN skill "deploy-ci" has 3 heuristics (below threshold = 5)
WHEN the promotion job runs
THEN "deploy-ci" is skipped (no LLM call)
```

---

## NEVER

- NEVER write promoted content directly to the active (non-quarantined) skill
- NEVER run promotion on the agent turn hot path — periodic job only
- NEVER re-evaluate a skill whose heuristic batch hash is already in `skill_heuristic_promotions`
- NEVER promote heuristics below `erl_min_confidence`
- NEVER hardcode a model name for the promotion provider
- NEVER count heuristics below `heuristic_promotion_threshold` as candidates

---

## Agent Boundaries

### Always (without asking)
- Write promotion drafts at `quarantined` trust
- Record evaluation results in `skill_heuristic_promotions` to prevent re-evaluation
- Use the quality provider for promotion LLM calls (offline, non-latency-sensitive)

### Ask First
- Changing the default `heuristic_promotion_threshold` (affects how often promotions trigger)
- Adding heuristic promotion to the real-time (per-turn) path

### Never
- Auto-promote heuristics into active skill bodies without user review
- Run promotion during an active agent turn

---

## Implementation Notes

- Implemented in commit #4523 (A6 initial implementation), with bugs fixed in commits #4535 (NULL `skill_name` in promotion scan, blocking I/O in async), #4539 (lifecycle and placement fixes)
- CLI `zeph skills promote-heuristics [--skill <name>]` is implemented
- DB migration 093 (`skill_heuristic_promotions` table) is applied
- Background task JoinHandle is tracked in `LifecycleState`

## See Also

- [[015-self-learning/spec]] — ERL section, `skill_heuristics` table, `erl_min_confidence`
- [[057-autoskill-versioned-merging/spec]] — Add/Merge/Discard flow for new_skill promotions
- [[005-skills/spec]] — SKILL.md format, trust governance
- [[001-system-invariants/spec]] — system contracts
