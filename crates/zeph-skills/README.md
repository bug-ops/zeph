# zeph-skills

[![Crates.io](https://img.shields.io/crates/v/zeph-skills)](https://crates.io/crates/zeph-skills)
[![docs.rs](https://img.shields.io/docsrs/zeph-skills)](https://docs.rs/zeph-skills)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

SKILL.md parser, registry, embedding matcher, and hot-reload for Zeph.

## Overview

Parses SKILL.md files (YAML frontmatter + markdown body) from the `.zeph/skills/` directory, maintains an in-memory registry with hot-reload support, and formats selected skills into LLM system prompts. Supports semantic matching via Qdrant embeddings and self-learning skill evolution with trust scoring. Multi-language feedback detection (7 languages) drives trust transitions across all skills.

## Key modules

| Module | Description |
|--------|-------------|
| `loader` | SKILL.md parser (YAML frontmatter + markdown) |
| `registry` | In-memory skill registry with hot-reload and lazy body loading |
| `matcher` | Async embedding-based skill matching with two-stage category filtering |
| `bm25` | In-memory BM25 inverted index; fused with cosine scores via Reciprocal Rank Fusion |
| `qdrant_matcher` | Qdrant-backed vector store for skill matching at scale (feature `qdrant`) |
| `evolution` | Self-learning skill generation and refinement; handles `FailureKind`-tagged rejections and triggers improvement cycles |
| `trust` | `SkillTrust` — trust levels and source provenance; pairs `SkillTrustLevel` (re-exported from `zeph-common`) with per-skill source tracking |
| `trust_score` | Bayesian Wilson-score re-ranking of match candidates (`posterior_weight`, `posterior_mean`, `rerank`) |
| `watcher` | Filesystem watcher for skill hot-reload |
| `prompt` | Skill-to-prompt formatting (`full`, `compact`, `auto` modes via `SkillPromptMode`); injects `reliability="N%"` and `uses="N"` health XML attributes |
| `manager` | `SkillManager` — install, remove, verify, and list external skills; `install_from_path` / `install_from_url` copy packages with `SkillTrustLevel::Quarantined` default and strip `.bundled` markers to prevent trust escalation |
| `rl_head` | `RoutingHead` — 2-layer MLP trained with REINFORCE for skill re-ranking; `ForwardCache` caches forward-pass activations for the gradient update; shared via `Arc<Mutex<_>>` and persisted to SQLite |
| `generator` | `SkillGenerator` — LLM-powered natural language skill generation from user descriptions; `SkillGenerationRequest` / `GeneratedSkill` types |
| `miner` | `SkillMiner` — GitHub repository mining for skill discovery (feature `miner`) |
| `stem` | STEM heuristic — detects repeated tool-call patterns and triggers automatic skill generation |
| `erl` | Experiential Reflective Learning — extracts heuristics from completed execution traces |
| `promoter` | Heuristic-to-skill promotion (`PromotionRecommendation`, `build_promotion_prompt`, `parse_promotion_response`) |
| `scanner` / `semantic_scanner` | Advisory prompt-injection pattern scanners for SKILL.md content |

**Re-exports:** `SkillError`, `SkillTrust`, `SkillSource`, `SkillTrustLevel` (from `zeph-common`), `compute_skill_hash`, plus matcher (`MatchResult`, `ScoredMatch`), generator, and evaluator types

## Prompt modes

The `prompt_mode` config option (`[skills]` section) controls how skills are serialized into the LLM system prompt:

| Mode | Description |
|------|-------------|
| `full` | Full XML format with complete skill body (default) |
| `compact` | Condensed XML with name, description, and trigger list only |
| `auto` | Selects `compact` when context budget is below threshold, `full` otherwise |

All modes include `reliability="N%"` and `uses="N"` XML attributes derived from the Wilson score posterior, so the model is aware of each skill's historical reliability.

## Self-learning and re-ranking

Skills accumulate outcomes over time. After each use, the Wilson score lower-bound is recomputed (via `trust_score::posterior_weight` / `posterior_mean`) and used to re-rank match candidates so that historically reliable skills surface first:

- Sufficient high-quality outcomes → higher posterior weight, promotion toward `Trusted`
- Repeated failures or rejections → deflated posterior weight, demotion toward `Quarantined`

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

D2Skill tracks step-level execution outcomes within multi-step skills. When a step fails, the error context and correction are persisted per skill in the `skill_step_corrections` table so subsequent invocations can pre-empt the same failure. Corrections are embedded and retrieved by cosine similarity during skill execution.

## SkillOrchestra RL routing head

`SkillOrchestra` uses a `RoutingHead` — a 2-layer MLP (`score = sigmoid(w2 @ relu(w1 @ input + b1) + b2)`) trained online with REINFORCE and a running reward baseline for variance reduction. Each candidate's feature vector is `query_embed ++ skill_embed ++ [cosine_score, success_rate, log_use_count]`. `RoutingHead::rerank` returns pure-cosine order until `warmup_updates` gradient updates have landed, then blends the MLP score with cosine similarity. Weights are shared via `Arc<Mutex<_>>` and persisted to SQLite (singleton-row; last-writer-wins across instances). Enable via `rl_routing_enabled = true` in `[skills]`.

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

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend for `zeph-memory` (only active when `qdrant` pulls `zeph-memory` in) |
| `postgres` | no | PostgreSQL backend for `zeph-memory` (only active when `qdrant` pulls `zeph-memory` in) |
| `qdrant` | no | Qdrant-backed semantic skill matching at scale (`qdrant_matcher`); pulls in `zeph-memory` + `qdrant-client` |
| `miner` | no | Builds the `zeph-skills-miner` binary for GitHub repository skill mining |
| `profiling` | no | Enables profiling instrumentation in the matcher hot path |

## Installation

```bash
cargo add zeph-skills
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
