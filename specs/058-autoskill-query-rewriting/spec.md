---
aliases:
  - AutoSkill A3
  - Query Rewriting for Skill Retrieval
  - Retrieval Query Rewriting
tags:
  - sdd
  - spec
  - skills
  - retrieval
  - autoskill
created: 2026-05-19
status: draft
related:
  - "[[005-skills/spec]]"
  - "[[015-self-learning/spec]]"
  - "[[001-system-invariants/spec]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: Query Rewriting Before Skill Retrieval (AutoSkill A3)

> [!info]
> GitHub Issue: #4449
> Priority: P4
> Crate: `zeph-skills`
>
> Before embedding the user's query for skill matching, apply an optional lightweight
> LLM rewrite step that converts the raw query into a retrieval-optimized form.
> Uses a fast provider (e.g., `qwen3:8b`). Controlled by a config flag — disabled by default.

## Overview

### Problem Statement

`SkillMatcher` currently embeds the raw user query directly and computes cosine similarity
against skill description embeddings. Raw queries are often:
- Too conversational or verbose for high-precision embedding retrieval
- Phrased around a specific context rather than a reusable capability
- Using different vocabulary than the skill's concise description

AutoSkill demonstrates that rewriting the query to a canonical capability form before
embedding improves retrieval recall, especially for skills with abstract descriptions.

### Goal

Add an optional pre-embedding rewrite step in `SkillMatcher::match_skills`. When
`query_rewrite_provider` is set, call a fast LLM to normalize the raw query into a
retrieval-friendly capability phrase before embedding. The rewrite runs synchronously but
must be bounded in latency (the fast provider is explicitly selected for this reason).

### Out of Scope

- Query rewriting for non-skill retrieval paths (memory search, tool schema filter)
- Caching rewritten queries between turns
- Using query rewriting as a substitute for BM25 (they are complementary)

---

## Key Invariants

- **Opt-in only**: `query_rewrite_provider = ""` (empty) disables rewriting entirely.
  The empty string is the default. Rewriting only activates when a non-empty provider name
  is configured.
- **Fast provider required**: query rewriting sits on the per-turn hot path. The configured
  provider MUST be a low-latency model (the config comment documents this constraint).
  There is no enforcement in code beyond the operator's provider choice.
- **Rewrite failure is non-fatal**: if the LLM call fails or times out, fall back to the
  raw query for embedding. NEVER abort skill matching due to a rewrite failure.
- **Raw query preserved**: the original user query is never mutated. The rewrite produces
  a separate string used only for the embedding call; the original is passed onward to the
  agent turn unchanged.
- **Provider resolved via registry**: `query_rewrite_provider` is a named `[[llm.providers]]`
  reference. Unknown name warns and disables rewriting for the turn. NEVER hardcode a model.
- **Rewrite is a single LLM call**: no multi-turn dialogue. One call, bounded prompt, one
  response (the rewritten query string). No tool use.

---

## Requirements

### Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `query_rewrite_provider` is non-empty, THE SYSTEM SHALL call the specified LLM provider with the raw user query and retrieve a rewritten capability phrase before embedding for skill matching | must |
| FR-002 | WHEN the rewrite LLM call fails (network error, timeout, invalid response), THE SYSTEM SHALL fall back to embedding the raw query and log at DEBUG level | must |
| FR-003 | THE SYSTEM SHALL use the rewritten query ONLY for the skill embedding lookup; the original query MUST be passed to the agent turn unchanged | must |
| FR-004 | WHEN `query_rewrite_provider` is empty or absent, THE SYSTEM SHALL skip the rewrite step and embed the raw query directly (existing behavior) | must |
| FR-005 | THE SYSTEM SHALL emit a tracing span `skills.query_rewrite` covering the LLM call duration | should |
| FR-006 | THE SYSTEM SHALL log the original and rewritten query at TRACE level for debugging | should |

### Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Latency | The rewrite LLM call MUST use a fast provider; operators SHOULD configure a <1B parameter local model or equivalent. The spec does not enforce a latency SLO but documents the design intent. |
| NFR-002 | Reliability | Rewrite failure MUST NOT cause skill matching to fail or return zero results |
| NFR-003 | Correctness | The raw query passed to the agent LLM MUST be identical before and after the rewrite step |

---

## SKILL.md Schema Changes

None. Query rewriting is a runtime matching optimization with no schema impact.

---

## Config Fields

All fields live under `[skills]`:

```toml
[skills]
# A3: Query rewriting before skill retrieval
# Set to a named [[llm.providers]] entry with a fast model (e.g., qwen3:8b).
# Empty string disables rewriting (default behavior).
query_rewrite_provider = ""   # named [[llm.providers]] name; empty = disabled
```

---

## Rewrite Prompt Contract

The rewrite prompt is a short system instruction + the raw user query. Example structure:

```
System: Convert the following user message into a concise retrieval query that describes
        the underlying capability or skill needed. Output only the rewritten query, no explanation.

User: <raw query>
```

The response is trimmed to the first non-empty line and used as the rewritten query string.
If the response is empty or only whitespace, fall back to the original query.

---

## Acceptance Criteria

```
GIVEN query_rewrite_provider = "" (default)
WHEN a user turn triggers skill matching
THEN the raw query is embedded directly with no additional LLM call

GIVEN query_rewrite_provider = "fast" (valid provider)
WHEN a user turn triggers skill matching
THEN a tracing span "skills.query_rewrite" appears in the trace
AND the embedding call receives the rewritten query, not the raw query
AND the message passed to the agent LLM is the original raw query

GIVEN query_rewrite_provider = "fast" AND the fast provider returns a network error
WHEN skill matching runs
THEN skill matching completes using the raw query as fallback
AND no error is surfaced to the user
AND a DEBUG log entry records the fallback

GIVEN query_rewrite_provider = "unknown-provider" (not in [[llm.providers]])
WHEN the agent starts
THEN a WARN log is emitted
AND query rewriting is disabled for all turns (treated as empty string)
```

---

## NEVER

- NEVER use the rewritten query as the actual user message in the conversation history
- NEVER block skill matching when the rewrite call fails — always fall back to raw query
- NEVER hardcode a model name in the rewrite path
- NEVER run a multi-step LLM dialogue for query rewriting — one call only
- NEVER use a quality/heavy provider for query rewriting — this path is per-turn hot

---

## Agent Boundaries

### Always (without asking)
- Preserve original user query unchanged throughout the turn
- Fall back to raw query on any rewrite failure

### Ask First
- Enabling query rewriting by default for all users (latency impact)
- Adding caching for rewritten queries

### Never
- Substitute the rewritten query for the original in any context other than the embedding call
- Use the quality provider (e.g., claude-opus) for query rewriting

---

## See Also

- [[005-skills/spec]] — `SkillMatcher`, matching algorithm, hybrid search
- [[015-self-learning/spec]] — BM25+RRF hybrid search pipeline
- [[059-autoskill-bm25-hybrid/spec]] — BM25 wiring (complementary to query rewriting)
- [[024-multi-model-design/spec]] — multi-model design principle, `*_provider` pattern
