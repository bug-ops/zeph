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

Skill generation uses the primary LLM provider by default. No additional configuration is needed beyond having skills enabled:

```toml
[skills]
paths = [".zeph/skills"]
```

## Next Steps

- [Add Custom Skills](../guides/custom-skills.md) — manual skill creation guide
- [Skills](../concepts/skills.md) — how skill matching works
- [Skill Trust Levels](skill-trust.md) — security model for generated skills
