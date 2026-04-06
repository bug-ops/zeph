# Natural Language Skill Generation

Zeph can generate new skills from a plain-text description or by mining existing GitHub repositories. This allows you to extend the agent's capabilities without writing SKILL.md files manually.

## Generate from Description

Use the `/skill create` command with a natural language description:

```
/skill create "A skill that formats JSON files using jq"
```

Zeph generates a complete SKILL.md with:

- YAML frontmatter (name, description, compatibility requirements)
- Instructions for the LLM
- Example commands and expected outputs
- Appropriate `allowed-tools` declarations

The generation uses LLM reflection: the model first reasons about what the skill needs to do, then produces the skill body. The result is saved to your skills directory and hot-reloaded immediately.

### Duplicate Detection

Before creating a new skill, Zeph checks semantic similarity against all existing skills in the registry. If a skill with similar functionality already exists (cosine similarity above threshold), the creation is rejected with a message explaining which existing skill overlaps.

This prevents skill bloat from near-duplicate definitions.

## Mine from GitHub Repositories

Zeph can analyze a GitHub repository and extract actionable skill definitions from its structure, README, and documentation:

```
/skill mine https://github.com/user/project
```

The mining process:

1. Clones the repository (shallow clone)
2. Analyzes README, docs, and project structure
3. Identifies distinct capabilities the repository provides
4. Generates one SKILL.md per identified capability
5. Runs duplicate detection against the existing skill registry
6. Saves non-duplicate skills to the managed directory

Generated skills start at the `quarantined` trust level. Review and promote them with:

```bash
zeph skill verify <name>
zeph skill trust <name> verified
```

## Sanitization

All generated skill content is sanitized before saving:

- Structural XML tags are escaped to prevent prompt injection
- URL domains in skill bodies are checked against a configurable allowlist
- Descriptions are capped at 2048 characters
- Skill names are validated against the naming rules (lowercase, hyphens, 1-64 chars)

## Configuration

Skill generation uses the primary LLM provider by default. The generation provider and output directory can be tuned independently from the main skill search paths:

```toml
[skills]
paths                = [".zeph/skills"]
generation_provider  = "quality"   # Provider for /skill create generation; empty = primary (default: "")
generation_output_dir = ".zeph/skills/generated"  # Where /skill create writes files; empty = first entry in paths (default: null)
```

### GitHub Repository Mining

The `[skills.mining]` block controls the automated `zeph skill mine` pipeline that discovers and imports skills from GitHub repositories:

```toml
[skills.mining]
queries              = ["topic:cli-tool language:rust stars:>100"]  # GitHub search queries (default: [])
max_repos_per_query  = 20        # Repos fetched per query; capped at 100 by GitHub API (default: 20)
dedup_threshold      = 0.85      # Cosine similarity threshold; skills above this vs. existing are skipped (default: 0.85)
output_dir           = ".zeph/skills/mined"  # Directory for mined skill files (default: null = first path)
generation_provider  = "quality" # Provider for skill SKILL.md generation during mining; empty = primary (default: "")
embedding_provider   = "fast"    # Provider for dedup embedding; empty = primary (default: "")
rate_limit_rpm       = 25        # Maximum GitHub API search requests per minute (default: 25)
```

`generation_provider` and `embedding_provider` should reference `[[llm.providers]]` entries. Using a fast, cheap model for `embedding_provider` and a capable model for `generation_provider` keeps mining cost low while producing high-quality SKILL.md output.

Lower `dedup_threshold` (e.g., `0.75`) aggressively deduplicates at the cost of occasionally rejecting genuinely distinct skills. The default `0.85` is a conservative threshold that catches near-duplicates without over-filtering.

## Next Steps

- [Add Custom Skills](../guides/custom-skills.md) — manual skill creation guide
- [Skills](../concepts/skills.md) — how skill matching works
- [Skill Trust Levels](skill-trust.md) — security model for generated skills
