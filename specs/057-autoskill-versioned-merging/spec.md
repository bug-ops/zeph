---
aliases:
  - AutoSkill A2
  - Versioned Skill Merging
  - Add/Merge/Discard Decision Flow
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
  - "[[056-autoskill-trace-extraction/spec]]"
  - "[[001-system-invariants/spec]]"
---

# Spec: Versioned Skill Merging with Add/Merge/Discard Decision Flow (AutoSkill A2)

> [!info]
> GitHub Issue: #4448
> Priority: P3
> Crate: `zeph-skills`
>
> Adds a `version` counter to SKILL.md frontmatter and implements a three-way
> Add/Merge/Discard decision flow for all newly proposed skill candidates.
> When a candidate is semantically similar to an existing skill, both are merged
> via LLM into a refined version rather than creating a duplicate or silently discarding.

## Overview

### Problem Statement

Zeph's current deduplication logic in the skill miner is binary: if a new skill's cosine
similarity to an existing skill exceeds `dedup_threshold` (default 0.90), the new skill is
discarded. If it is below the threshold, it is created as a separate skill. This approach:

1. Discards potentially complementary information that could enrich the existing skill
2. Allows near-duplicates at similarities between ~0.75 and ~0.90 to proliferate
3. Has no versioning, so skill improvements are invisible to users and tooling
4. Does not generalize to the conversation trace extraction pipeline (spec 056)

AutoSkill demonstrates that versioned merging — semantically unifying a candidate with its
nearest neighbor — produces richer skills over time (example: `professional_text_rewrite`
reached v0.1.34 through 34 refinements without human curation).

### Goal

Introduce a `version: u32` field to SKILL.md frontmatter. Implement Add/Merge/Discard
decision logic gated on configurable similarity thresholds. Apply this logic uniformly
across all skill creation paths: NL generation, GitHub mining, and trace extraction.

### Out of Scope

- Automatic trust promotion based on version number
- Version history storage (only the current version is stored)
- UI for viewing version diff between versions
- Merging between skills that are not nearest neighbors

---

## Key Invariants

- **Version 0 is the initial state** for all newly created skills; existing skills without
  a `version` field are treated as version 0 on load.
- **Merge preserves the existing skill on failure**: if the LLM merge call fails or the
  merged output fails injection sanitization, the existing skill is left UNCHANGED and the
  candidate is discarded. Never destroy existing skill content on a merge attempt.
- **Merge writes at `quarantined` trust**: merged results are initially quarantined
  and require user review before replacing the existing version in active use.
- **Thresholds are ordered**: `merge_threshold` MUST be strictly less than `dedup_threshold`.
  The three regions are:
  - `sim >= dedup_threshold` → candidate is an exact/near-exact duplicate → Discard
  - `merge_threshold <= sim < dedup_threshold` → semantically related → Merge (LLM unification)
  - `sim < merge_threshold` → genuinely novel → Add (create new quarantined skill)
- **Monotonically incrementing**: merged version number = existing version + 1. Never
  decrement or reset the version counter.
- **Provider must be configurable**: merge LLM calls use `skill_merge_provider` from
  `[[llm.providers]]`. Empty = primary provider. NEVER hardcode a model.
- **Applies to all creation paths**: NL generation (`/skill create`), GitHub mining
  (`/skill mine`), and trace extraction (spec 056). The same threshold logic governs all paths.

---

## Requirements

### Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL add a `version: u32` field to SKILL.md frontmatter; all existing skills without this field SHALL be treated as version 0 on load | must |
| FR-002 | WHEN a new skill candidate is proposed, THE SYSTEM SHALL compute cosine similarity against the nearest existing skill using the embedding provider | must |
| FR-003 | WHEN `sim >= dedup_threshold`, THE SYSTEM SHALL discard the candidate and log at DEBUG level | must |
| FR-004 | WHEN `merge_threshold <= sim < dedup_threshold`, THE SYSTEM SHALL call the LLM merge prompt with both skill bodies and produce a unified result with `version = existing.version + 1` | must |
| FR-005 | WHEN `sim < merge_threshold`, THE SYSTEM SHALL create a new quarantined SKILL.md for the candidate with `version = 0` | must |
| FR-006 | WHEN an LLM merge call fails or the merged output fails injection sanitization, THE SYSTEM SHALL leave the existing skill unchanged, discard the candidate, and log at WARN level | must |
| FR-007 | THE SYSTEM SHALL save merged results as quarantined drafts pending user review | must |
| FR-008 | THE SYSTEM SHALL emit a TUI notification when a merge candidate is produced, including the names of the two merged skills and the new version number | should |
| FR-009 | THE SYSTEM SHALL expose `/skill versions <name>` command to display the current version number and last-modified timestamp of a named skill | should |

### Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Reliability | Merge failure MUST leave the existing skill corpus unchanged — no partial writes |
| NFR-002 | Latency | Merge LLM call is background async — MUST NOT block the agent turn or skill creation response to the user |
| NFR-003 | Security | Merged skill body MUST pass injection sanitization before being written |
| NFR-004 | Observability | Tracing spans under `skills.merge.*` for cosine check, LLM call, and write |

---

## SKILL.md Schema Changes

### New `version` Field

```yaml
---
name: my-skill
description: A skill that rewrites text in a professional tone
version: 3          # NEW: monotonically incrementing; 0 for new skills
source: trace_extraction
---
```

- Field type: `u32`, default `0`
- Parsed from SKILL.md frontmatter by `SkillMeta`
- Written back to frontmatter on every merge

### Migration for Existing Skills

Existing SKILL.md files without `version` continue to load without error; the parsed value
defaults to `0`. No bulk file migration is required. The `version` field is written the first
time a skill is merged.

---

## Config Fields

All fields live under `[skills.learning]`:

```toml
[skills.learning]
# A2: Versioned merging
skill_merge_enabled = true                  # if false, only Add/Discard (no Merge)
skill_merge_provider = ""                   # named [[llm.providers]] name; empty = primary
merge_threshold = 0.75                      # sim >= merge_threshold → merge with nearest skill
# Note: dedup_threshold is defined in [skills] (existing field, default 0.90)
# Invariant: merge_threshold < dedup_threshold must hold; startup validation enforces this
```

- `skill_merge_enabled = false` disables the Merge branch entirely; similarity ≥ `merge_threshold`
  falls through to Discard (same as current behavior for sim ≥ `dedup_threshold`).
- Startup validation: if `merge_threshold >= dedup_threshold`, log ERROR and set
  `skill_merge_enabled = false` automatically.

---

## LLM Merge Prompt Contract

The merge prompt receives:
1. The existing skill body (full SKILL.md, sanitized)
2. The candidate skill body (full SKILL.md, sanitized)
3. Instruction: produce a unified SKILL.md that retains all distinct capabilities from both,
   removes redundancy, preserves the existing skill's `name` and `version + 1`

The LLM response must be a valid SKILL.md. If parsing fails, the merge is treated as a
failure (see FR-006).

---

## Acceptance Criteria

```
GIVEN an existing skill "rewrite-text" at version 2
AND a new candidate with cosine similarity 0.80 to "rewrite-text"
AND merge_threshold = 0.75 AND dedup_threshold = 0.90
WHEN the merge decision is evaluated
THEN the LLM merge prompt is called with both skill bodies
AND the result is saved as quarantined "rewrite-text" at version 3
AND the original "rewrite-text" at version 2 remains in the registry unchanged until user approves

GIVEN an existing skill "deploy-ci" at version 1
AND a new candidate with cosine similarity 0.95
WHEN the merge decision is evaluated
THEN the candidate is discarded (sim >= dedup_threshold)
AND no new skill file is written

GIVEN a new candidate with cosine similarity 0.40 to all existing skills
WHEN the merge decision is evaluated
THEN a new quarantined SKILL.md is created with version = 0

GIVEN the LLM merge call returns an invalid SKILL.md
WHEN the merge result is validated
THEN the existing skill is left unchanged
AND the candidate is discarded with a WARN log

GIVEN skill_merge_enabled = false
WHEN a candidate with similarity 0.80 is evaluated
THEN the candidate is discarded (falls through to Discard branch)
```

---

## NEVER

- NEVER overwrite the existing active skill with the merged result without user approval
- NEVER decrement or reset the `version` counter
- NEVER allow `merge_threshold >= dedup_threshold` — enforce at startup
- NEVER run the merge LLM call synchronously on the agent turn hot path
- NEVER hardcode a model name for the merge provider
- NEVER write a merged skill that fails injection sanitization
- NEVER propagate trust level from the existing skill to the merged quarantined draft

---

## Agent Boundaries

### Always (without asking)
- Default `version = 0` for skills without the field
- Write merged results at `quarantined` trust
- Leave existing skill unchanged on merge failure

### Ask First
- Changing `merge_threshold` default (affects corpus merge rate)
- Adding version history / diff storage

### Never
- Auto-approve merged skills without user review
- Merge skills that are not nearest neighbors
- Set merged version to anything other than `existing.version + 1`

---

## See Also

- [[005-skills/spec]] — SKILL.md format, dedup_threshold, trust governance
- [[015-self-learning/spec]] — trust model, ARISE, ERL
- [[056-autoskill-trace-extraction/spec]] — source of trace-extracted candidates
- [[001-system-invariants/spec]] — system contracts
