# zeph-skills

[![Crates.io](https://img.shields.io/crates/v/zeph-skills)](https://crates.io/crates/zeph-skills)
[![docs.rs](https://img.shields.io/docsrs/zeph-skills)](https://docs.rs/zeph-skills)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

SKILL.md parser, registry, embedding matcher, and hot-reload for Zeph.

## Overview

Parses SKILL.md files (YAML frontmatter + markdown body) from the `.zeph/skills/` directory, maintains an in-memory registry with hot-reload support, and formats selected skills into LLM system prompts. Supports semantic matching via Qdrant embeddings and self-learning skill evolution with trust scoring. Multi-language feedback detection (7 languages) drives trust transitions across all skills.

## Key modules

| Module | Description |
|--------|-------------|
| `loader` | SKILL.md parser (YAML frontmatter + markdown) |
| `registry` | In-memory skill registry with hot-reload |
| `matcher` | Keyword-based skill matching |
| `qdrant_matcher` | Semantic skill matching via Qdrant; BM25 hybrid search with RRF fusion when `hybrid_search = true` |
| `evolution` | Self-learning skill generation and refinement; handles `FailureKind`-tagged rejections and triggers improvement cycles |
| `trust` | `SkillTrust` — Wilson score Bayesian re-ranking (`posterior_weight`, `posterior_mean`); `check_trust_transition()` for auto-promote/demote; re-exports `TrustLevel` from `zeph-tools` |
| `watcher` | Filesystem watcher for skill hot-reload |
| `prompt` | Skill-to-prompt formatting (`full`, `compact`, `auto` modes via `SkillPromptMode`); injects `reliability="N%"` and `uses="N"` health XML attributes |
| `manager` | `SkillManager` — install, remove, verify, and list external skills (`~/.config/zeph/skills/`); `install_from_path` / `install_from_url` copy packages with `TrustLevel::Quarantined` default and strip `.bundled` markers to prevent trust escalation |
| `rl_head` | `RoutingHead` — LinUCB bandit for RL-based skill routing; `ForwardCache` for score memoization; serializable to/from bytes for SQLite persistence |
| `generator` | `SkillGenerator` — LLM-powered natural language skill generation from user descriptions; `SkillGenerationRequest` / `GeneratedSkill` types |
| `miner` | `SkillMiner` — GitHub repository mining for skill discovery; `RepoCandidate`, `MinedSkill`, `MiningConfig` |
| `stem` | STEM heuristic — `should_generate_skill` detects repeated tool-call patterns and triggers skill generation; `ToolPattern` tracks per-sequence success rates |
| `erl` | ERL (Experience Reflection Learning) — `build_reflection_extract_prompt` extracts heuristics from past execution traces; `text_similarity` for deduplication |
| `scanner` | Injection sanitization — detects and mitigates prompt injection patterns in SKILL.md content |

**Re-exports:** `SkillError`, `SkillTrust`, `TrustLevel` (from `zeph-tools`), `compute_skill_hash`

## Prompt modes

The `prompt_mode` config option (`[skills]` section) controls how skills are serialized into the LLM system prompt:

| Mode | Description |
|------|-------------|
| `full` | Full XML format with complete skill body (default) |
| `compact` | Condensed XML with name, description, and trigger list only |
| `auto` | Selects `compact` when context budget is below threshold, `full` otherwise |

All modes include `reliability="N%"` and `uses="N"` XML attributes derived from the Wilson score posterior, so the model is aware of each skill's historical reliability.

## Self-learning and re-ranking

Skills accumulate outcomes over time. After each use, the Wilson score lower-bound is recomputed and stored as `posterior_weight` and `posterior_mean`. `check_trust_transition()` evaluates whether accumulated evidence justifies a trust level change:

- Sufficient high-quality outcomes → promote toward `Trusted`
- Repeated failures or rejections → demote toward `Quarantined`

The `/skill reject <name> <reason>` command records a typed `FailureKind` rejection immediately, persisting it to the `outcome_detail` column (migration 018).

Feedback signals are detected by `FeedbackDetector` in `zeph-core`, which now supports 7 languages (English, Russian, Spanish, German, French, Portuguese, Chinese). Multi-language implicit correction detection drives skill trust transitions regardless of the user's language.

## Hybrid search configuration

```toml
[skills]
cosine_weight            = 0.7   # weight of cosine similarity in RRF fusion (default: 0.7)
hybrid_search            = true  # enable BM25 + cosine hybrid search (default: true)
disambiguation_threshold = 0.20  # minimum score gap for skill disambiguation (default: 0.20, was 0.05 before v0.18.2)
min_injection_score      = 0.20  # minimum match score for skill injection into the prompt (default: 0.20)
```

**Note:** `disambiguation_threshold` default changed from 0.05 to 0.20 in v0.18.2 — this reduces false-positive skill injections for low-confidence queries. `min_injection_score` is a new field that gates injection independently of disambiguation.

**Note:** When `hybrid_search = true`, BM25 keyword scores are computed locally and fused with Qdrant cosine scores using Reciprocal Rank Fusion. This improves recall for exact-match queries while preserving semantic ranking quality for paraphrase queries.

## D2Skill step-level error correction

D2Skill (Dynamic Diagnostic Skill) tracks step-level execution outcomes within multi-step skills. When a step fails, the error context and correction are persisted via `insert_step_correction` so subsequent invocations can pre-empt the same failure. Corrections are embedded and retrieved by cosine similarity during skill execution.

## SkillOrchestra RL routing head

`SkillOrchestra` uses a `RoutingHead` (LinUCB contextual bandit) to select which skill variant to invoke. The routing head maintains per-skill weight vectors and a shared baseline; after each invocation, it updates weights based on the observed reward signal. Enable via `rl_routing_enabled = true` in `[skills]`.

```toml
[skills]
rl_routing_enabled = true
```

## NL skill generation and GitHub repo mining

`SkillGenerator` accepts a natural language description and produces a complete SKILL.md file via LLM generation. `SkillMiner` searches GitHub repositories for tool-use patterns and converts them into candidate skills. The STEM heuristic (`should_generate_skill`) monitors tool-call sequences and triggers automatic skill generation when a pattern recurs above a configurable threshold.

## Confusability detection

`SkillMatcher::confusability_report` identifies pairs of skills whose embeddings are close enough to cause ambiguous matching. The report includes the cosine similarity score and both skill names, helping skill authors disambiguate overlapping definitions.

## Injection sanitization

The `scanner` module detects prompt injection patterns in SKILL.md content at load time. Detected patterns are flagged and mitigated before the skill enters the registry.

## Installation

```bash
cargo add zeph-skills
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
