# zeph-skills

[![Crates.io](https://img.shields.io/crates/v/zeph-skills)](https://crates.io/crates/zeph-skills)
[![docs.rs](https://img.shields.io/docsrs/zeph-skills)](https://docs.rs/zeph-skills)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

SKILL.md parser, registry, embedding matcher, and hot-reload for Zeph.

## Overview

Parses SKILL.md files (YAML frontmatter + markdown body) from the `skills/` directory, maintains an in-memory registry with hot-reload support, and formats selected skills into LLM system prompts. Supports semantic matching via Qdrant embeddings and self-learning skill evolution with trust scoring.

## Key modules

| Module | Description |
|--------|-------------|
| `loader` | SKILL.md parser (YAML frontmatter + markdown) |
| `registry` | In-memory skill registry with hot-reload |
| `matcher` | Keyword-based skill matching |
| `qdrant_matcher` | Semantic skill matching via Qdrant |
| `evolution` | Self-learning skill generation and refinement |
| `trust` | `SkillTrust`, `TrustLevel` — skill trust scoring |
| `watcher` | Filesystem watcher for skill hot-reload |
| `prompt` | Skill-to-prompt formatting (`full`, `compact`, `auto` modes via `SkillPromptMode`) |
| `manager` | `SkillManager` — install, remove, verify, and list external skills (`~/.config/zeph/skills/`) |

**Re-exports:** `SkillError`, `SkillTrust`, `TrustLevel`, `compute_skill_hash`

## Prompt modes

The `prompt_mode` config option (`[skills]` section) controls how skills are serialized into the LLM system prompt:

| Mode | Description |
|------|-------------|
| `full` | Full XML format with complete skill body (default) |
| `compact` | Condensed XML with name, description, and trigger list only |
| `auto` | Selects `compact` when context budget is below threshold, `full` otherwise |

## Installation

```bash
cargo add zeph-skills
```

## License

MIT
