# Add Custom Skills

Create your own skills to teach Zeph new capabilities. A skill is a single `SKILL.md` file inside a named directory.

## Skill Structure

```text
skills/
└── my-skill/
    └── SKILL.md
```

## SKILL.md Format

Two parts: a YAML header and a markdown body.

```markdown
---
name: my-skill
description: Short description of what this skill does.
---
# My Skill

Instructions and examples go here. This content is injected verbatim
into the LLM context when the skill is matched.
```

### Header Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Unique identifier (1-64 chars, lowercase, hyphens allowed) |
| `description` | Yes | Used for embedding-based matching against user queries |
| `compatibility` | No | Runtime requirements (e.g., "requires curl") |
| `allowed-tools` | No | Comma-separated tool names this skill can use |

### Name Rules

Lowercase letters, numbers, and hyphens only. No leading, trailing, or consecutive hyphens. Must match the directory name.

## Skill Resources

Add reference files alongside `SKILL.md`:

```text
skills/
└── system-info/
    ├── SKILL.md
    └── references/
        ├── linux.md
        ├── macos.md
        └── windows.md
```

Resources in `scripts/`, `references/`, and `assets/` are loaded on demand. OS-specific files (`linux.md`, `macos.md`, `windows.md`) are filtered by platform automatically.

## Configuration

```toml
[skills]
paths = ["./skills", "/home/user/my-skills"]
max_active_skills = 5
```

Skills from multiple paths are scanned. If a skill with the same name appears in multiple paths, the first one found takes priority.

## Testing Your Skill

1. Place the skill directory under `./skills/`
2. Start Zeph — the skill is loaded automatically
3. Send a message that should match your skill's description
4. Run `/skills` to verify it was selected

Changes to `SKILL.md` are hot-reloaded without restart (500ms debounce).

## Deep Dives

- [Skills](../concepts/skills.md) — how embedding-based matching works
- [Self-Learning Skills](../advanced/self-learning.md) — automatic skill evolution
- [Skill Trust Levels](../advanced/skill-trust.md) — security model for imported skills
