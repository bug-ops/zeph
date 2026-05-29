---
aliases:
  - AutoSkill A4
  - BM25 Hybrid Retrieval
  - Hybrid Dense Lexical Retrieval
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

# Spec: Wire BM25 into SkillMatcher for Hybrid Dense+Lexical Retrieval (AutoSkill A4)

> [!info]
> GitHub Issue: #4450
> Priority: P4
> Crate: `zeph-skills`
>
> `bm25.rs` already exists in `crates/zeph-skills/src/`. Wire it into `SkillMatcher`
> as a second retrieval signal and fuse with cosine similarity scores using a configurable
> alpha weight. Addresses vocabulary-mismatch retrieval failures where the user uses
> terms not present in skill description embeddings.

## Overview

### Problem Statement

`SkillMatcher` already has a `hybrid_search` config flag and `bm25.rs` exists in the
crate. However, the 005-skills spec documents the BM25 path as already implemented
("`BM25 + embedding hybrid` (if `hybrid_search = true`)"). This spec clarifies the exact
fusion formula, ensures `bm25.rs` is correctly wired into the retrieval pipeline in
alignment with AutoSkill's approach, and documents the scoring contract for implementors.

> [!note]
> The 005-skills spec already describes BM25+RRF as the hybrid path. This spec exists
> to (1) confirm the wiring matches the AutoSkill alpha-fusion model, (2) document the
> BM25 index rebuild contract, and (3) specify the config fields and acceptance criteria
> for the specific behavior requested in #4450 if the current implementation deviates.

### Goal

Ensure `SkillMatcher` uses a linear alpha-weighted fusion of normalized BM25 and cosine
similarity scores when `hybrid_search = true`, with the BM25 index built from skill
descriptions at matcher construction time and rebuilt on hot-reload. Provide a
configurable `hybrid_alpha` weight.

### Out of Scope

- BM25 for non-skill retrieval paths
- Replacing RRF with alpha-fusion globally (this spec targets skills matching only)
- Per-query BM25 index updates (index is rebuilt at construction/reload only)

---

## Key Invariants

- **`bm25.rs` is not rewritten**: the existing `bm25.rs` module is used as-is. This spec
  wires it into `SkillMatcher`; it does not replace or redesign the BM25 implementation.
- **Index built at construction**: the BM25 index is built once from all skill descriptions
  when `SkillMatcher` is constructed, and rebuilt on every hot-reload. It is NOT updated
  per query.
- **`hybrid_search = false` path unchanged**: when disabled, skill matching uses pure
  cosine similarity only. No behavior change on the default path.
- **Normalized scores before fusion**: BM25 scores MUST be normalized to [0.0, 1.0] before
  the alpha-fusion formula is applied. Raw BM25 term-frequency scores are not on the same
  scale as cosine similarity.
- **Alpha semantics**: `hybrid_alpha = 1.0` = pure cosine (embedding only); `hybrid_alpha = 0.0`
  = pure BM25 (lexical only). Default `0.7` gives priority to dense semantic matching.
- **Wilson score re-ranking applies after fusion**: the final RRF/alpha fused score is
  multiplied by the Wilson trust weight before final ranking. This ordering MUST be preserved.
- **No LLM calls**: BM25 is a pure statistical signal. No provider is involved.

---

## Requirements

### Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `hybrid_search = true`, THE SYSTEM SHALL fuse BM25 score and cosine similarity using `score = hybrid_alpha * cosine + (1 - hybrid_alpha) * bm25_normalized` | must |
| FR-002 | THE SYSTEM SHALL build the BM25 index from all skill descriptions at `SkillMatcher` construction time | must |
| FR-003 | THE SYSTEM SHALL rebuild the BM25 index whenever the skill registry hot-reloads | must |
| FR-004 | THE SYSTEM SHALL normalize BM25 scores to [0.0, 1.0] before fusion; scores that cannot be normalized (e.g., zero-document index) SHALL be treated as 0.0 | must |
| FR-005 | WHEN `hybrid_search = false`, THE SYSTEM SHALL use pure cosine similarity (existing behavior unchanged) | must |
| FR-006 | THE SYSTEM SHALL apply Wilson score re-ranking AFTER the hybrid fusion score is computed | must |
| FR-007 | THE SYSTEM SHALL log the BM25 and cosine component scores at TRACE level for each candidate when hybrid search is active | should |

### Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Latency | BM25 index lookup MUST complete in <5ms for a skill corpus of ≤500 skills on typical hardware |
| NFR-002 | Correctness | `hybrid_search = false` path MUST produce identical results to the current implementation |
| NFR-003 | Reliability | BM25 index build failure MUST fall back to pure cosine matching with a WARN log |

---

## SKILL.md Schema Changes

None. BM25 is indexed from existing `description` and `triggers` fields.

---

## Config Fields

All fields live under `[skills]`:

```toml
[skills]
hybrid_search = false           # existing field; enables BM25+cosine fusion
hybrid_alpha = 0.7              # NEW: fusion weight; 1.0 = pure cosine, 0.0 = pure BM25
```

- `hybrid_alpha` range: `[0.0, 1.0]`. Values outside this range are clamped at startup
  with a WARN log.
- `hybrid_alpha` only takes effect when `hybrid_search = true`.

---

## Fusion Formula

```
bm25_normalized_i = bm25_score_i / max(bm25_scores)   // max across all candidates
final_score_i = hybrid_alpha * cosine_i + (1.0 - hybrid_alpha) * bm25_normalized_i
ranked_score_i = final_score_i * wilson_lower_i
```

When `max(bm25_scores) == 0.0` (no term overlap for any candidate), `bm25_normalized_i = 0.0`
for all candidates and the formula reduces to `hybrid_alpha * cosine_i`.

---

## Acceptance Criteria

```
GIVEN hybrid_search = false (default)
WHEN skill matching runs
THEN only cosine similarity is used; bm25.rs code is NOT called
AND results are identical to current behavior

GIVEN hybrid_search = true AND hybrid_alpha = 0.7
WHEN a user query contains exact terms from a skill description
THEN the BM25 signal amplifies that skill's score relative to semantically-similar
     but lexically-dissimilar competitors

GIVEN hybrid_search = true AND a new skill is hot-reloaded
WHEN the registry reload event fires
THEN the BM25 index is rebuilt to include the new skill's description

GIVEN hybrid_alpha = 1.5 (out of range)
WHEN the agent starts
THEN a WARN log is emitted and hybrid_alpha is clamped to 1.0

GIVEN BM25 index build throws an error during SkillMatcher construction
WHEN hybrid_search = true
THEN a WARN log is emitted
AND SkillMatcher falls back to pure cosine matching for all turns until next reload
```

---

## NEVER

- NEVER update the BM25 index per query — only at construction/reload
- NEVER skip Wilson score re-ranking after fusion
- NEVER allow `hybrid_alpha` outside [0.0, 1.0] without clamping
- NEVER call an LLM in the BM25 retrieval path
- NEVER modify `bm25.rs` unless fixing a bug — this spec wires it, not rewrites it

---

## Agent Boundaries

### Always (without asking)
- Rebuild BM25 index on hot-reload
- Apply Wilson score after fusion
- Clamp `hybrid_alpha` to [0.0, 1.0]

### Ask First
- Changing the fusion formula from alpha-weighted to RRF
- Changing which skill fields are indexed in BM25 (currently: description + triggers)

### Never
- Enable hybrid search by default without A/B validation showing no regression
- Update BM25 index during a live turn

---

## Implementation Notes

- Implemented in commit #4506 (A3+A4+A5 combined PR)
- `matched_indices` is re-synced with `active_skills` after the channel allowlist filter to prevent index drift (commits #4435, #4437)
- `xml_escape` function consolidated into `zeph-common` (commit #4437)

## See Also

- [[005-skills/spec]] — `SkillMatcher`, hybrid_search, Wilson score re-ranking
- [[015-self-learning/spec]] — BM25+RRF hybrid search pipeline
- [[058-autoskill-query-rewriting/spec]] — complementary query rewriting (A3)
- [[060-autoskill-trigger-sets/spec]] — trigger set indexing (A5)
