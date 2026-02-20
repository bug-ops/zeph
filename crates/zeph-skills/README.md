# zeph-skills

SKILL.md loader, skill registry, skill manager, and prompt formatter.

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
| `prompt` | Skill-to-prompt formatting |
| `manager` | `SkillManager` — install, remove, verify, and list external skills (`~/.config/zeph/skills/`) |

**Re-exports:** `SkillError`, `SkillTrust`, `TrustLevel`, `compute_skill_hash`

## Usage

```toml
[dependencies]
zeph-skills = { path = "../zeph-skills" }
```

## License

MIT
