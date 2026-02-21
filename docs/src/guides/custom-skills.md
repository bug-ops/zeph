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
| `allowed-tools` | No | Space-separated tool names this skill can use |
| `requires-secrets` | No | Comma-separated secret names the skill needs (see below) |

### Secret-Gated Skills

If a skill requires API credentials or tokens, declare them with `requires-secrets`:

```markdown
---
name: github-api
description: GitHub API integration — search repos, create issues, review PRs.
requires-secrets: github-token, github-org
---
```

Secret names use lowercase with hyphens. They map to vault keys with the `ZEPH_SECRET_` prefix:

| `requires-secrets` name | Vault key | Env var injected |
|------------------------|-----------|-----------------|
| `github-token` | `ZEPH_SECRET_GITHUB_TOKEN` | `GITHUB_TOKEN` |
| `github-org` | `ZEPH_SECRET_GITHUB_ORG` | `GITHUB_ORG` |

**Activation gate:** if any declared secret is missing from the vault, the skill is excluded from the prompt. It will not be matched or suggested until the secret is provided.

**Scoped injection:** when the skill is active, its secrets are injected as environment variables into shell commands the skill executes. Only the secrets declared by the active skill are exposed — not all vault secrets.

Store secrets with the vault CLI:

```bash
zeph vault set ZEPH_SECRET_GITHUB_TOKEN ghp_yourtokenhere
zeph vault set ZEPH_SECRET_GITHUB_ORG my-org
```

See [Vault — Custom Secrets](../reference/security.md#custom-secrets) for full details.

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

## Installing External Skills

Use `zeph skill install` to add skills from git repositories or local paths:

```bash
# From a git URL — clones the repo into ~/.config/zeph/skills/
zeph skill install https://github.com/user/zeph-skill-example.git

# From a local path — copies the skill directory
zeph skill install /path/to/my-skill
```

Installed skills are placed in `~/.config/zeph/skills/` and automatically discovered at startup. They start at the `quarantined` trust level (restricted tool access). To grant full access:

```bash
zeph skill verify my-skill        # check BLAKE3 integrity
zeph skill trust my-skill trusted  # promote trust level
```

In an active session, use `/skill install <url|path>` and `/skill remove <name>` — changes are hot-reloaded without restart.

See [Skill Trust Levels](../advanced/skill-trust.md) for the full security model.

## Deep Dives

- [Skills](../concepts/skills.md) — how embedding-based matching works
- [Self-Learning Skills](../advanced/self-learning.md) — automatic skill evolution
- [Skill Trust Levels](../advanced/skill-trust.md) — security model for imported skills
