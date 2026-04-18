# Add Custom Skills

Create your own skills to teach Zeph new capabilities. A skill is a single `SKILL.md` file inside a named directory.

## Skill Structure

```text
.zeph/skills/
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
| `x-requires-secrets` | No | Comma-separated secret names the skill needs (see below) |

### Secret-Gated Skills

If a skill requires API credentials or tokens, declare them with `x-requires-secrets`:

```markdown
---
name: github-api
description: GitHub API integration — search repos, create issues, review PRs.
x-requires-secrets: github-token, github-org
---
```

Secret names use lowercase with hyphens. They map to vault keys with the `ZEPH_SECRET_` prefix:

| `x-requires-secrets` name | Vault key | Env var injected |
|--------------------------|-----------|-----------------|
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

### Channel Allowlist

Restrict a skill to specific I/O channels with `x-channels`. When set, the skill is excluded from matching on channels not in the list:

```markdown
---
name: deploy-prod
description: Production deployment via kubectl.
x-channels: cli
---
```

This skill only activates in CLI mode — it is invisible in Telegram or TUI. Omit `x-channels` to allow all channels. Multiple channels are comma-separated: `x-channels: cli, tui`.

### Name Rules

Lowercase letters, numbers, and hyphens only. No leading, trailing, or consecutive hyphens. Must match the directory name.

## Skill Resources

Add reference files alongside `SKILL.md`:

```text
.zeph/skills/
└── system-info/
    ├── SKILL.md
    └── references/
        ├── linux.md
        ├── macos.md
        └── windows.md
```

Resources in `scripts/`, `references/`, and `assets/` are loaded lazily on first skill activation (not at startup). OS-specific files (`linux.md`, `macos.md`, `windows.md`) are filtered by platform automatically.

Local file references in the skill body (e.g., `[see config](references/config.md)`) are validated at load time. Broken links and path traversal attempts (`../../../etc/passwd`) are rejected.

## Configuration

```toml
[skills]
paths = [".zeph/skills", "/home/user/my-skills"]
max_active_skills = 5
```

Skills from multiple paths are scanned. If a skill with the same name appears in multiple paths, the first one found takes priority.

## Testing Your Skill

1. Place the skill directory under `.zeph/skills/`
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

## Plugin Packages

For distributing and managing multiple related skills, utilities, and configurations together, Zeph supports **plugin packages**. A plugin is a directory containing a `plugin.toml` manifest that bundles:

- Multiple skill directories
- MCP server entries
- Configuration overlays (tighten-only: you can only restrict, not expand permissions)

### Plugin Structure

```text
my-plugin/
├── plugin.toml                 # Manifest file
├── skills/
│   ├── skill-one/
│   │   └── SKILL.md
│   └── skill-two/
│       └── SKILL.md
└── config/
    └── overlay.toml            # Optional config tightening rules
```

### plugin.toml Format

```toml
[plugin]
name = "my-plugin"
version = "1.0.0"
description = "My plugin description"

# Skills bundled with this plugin (relative paths from plugin root)
[[plugin.skills]]
name = "skill-one"
path = "skills/skill-one"

[[plugin.skills]]
name = "skill-two"
path = "skills/skill-two"

# MCP servers managed by this plugin (optional)
[[plugin.mcp_servers]]
id = "my-mcp-server"
command = "python"
args = ["-m", "my_mcp_module"]

# Configuration overlay — restrictive only (default: empty)
[plugin.config_overlay]
# Union of blocked patterns:
tools.blocked_commands = ["dangerous_pattern"]

# Intersection of allowed patterns (if base is empty, stays empty):
# tools.allowed_commands = ["safe_pattern"]

# Maximum for numeric fields:
# skills.disambiguation_threshold = 0.1
```

### Installing Plugins

Use `zeph plugin add` to install a plugin from a local path:

```bash
# From local directory
zeph plugin add /path/to/my-plugin

# List installed plugins
zeph plugin list

# Show the active plugin overlay (which plugins are active/skipped and why)
zeph plugin list --overlay

# Remove a plugin
zeph plugin remove my-plugin
```

Plugins are installed to `~/.local/share/zeph/plugins/<name>/` (XDG standard location). All bundled skills are automatically discovered and hot-reloaded without restart.

**In TUI mode**, use the `/plugins` commands:

```
/plugins list              # Show installed plugins
/plugins list --overlay    # Show the active plugin overlay
/plugins overlay           # Show the active plugin overlay (alias)
/plugins add <path>        # Install a plugin
/plugins remove <name>     # Remove a plugin
```

### Plugin Integrity Check

When you install a plugin, Zeph records a sha256 digest of its `.plugin.toml` manifest in `~/.local/share/zeph/.plugin-integrity.toml`. At startup and when hot-reloading, Zeph verifies this digest to detect if a plugin manifest has been modified outside of Zeph's control.

**If a manifest is tampered with:**
- The plugin is skipped with an "integrity mismatch" reason
- You can see the skipped plugin and reason with `zeph plugin list --overlay` or `/plugins overlay`
- To re-protect the plugin, reinstall it: `zeph plugin remove my-plugin && zeph plugin add /path/to/my-plugin`

This provides basic tampering detection. The integrity check is not cryptographically signed, and concurrent installs may race (last writer wins).

### Hot-Reload Behavior

Plugin config overlays — restrictions on tool access and embedding thresholds — are applied immediately when a plugin is installed or when you reload config mid-session. However, different overlay fields hot-reload differently:

**Hot-reloads live (no restart needed):**
- `tools.blocked_commands` — shell commands blocked by the agent are updated atomically on the next execution

**Require agent restart:**
- `tools.allowed_commands` — restrictions on allowed paths are applied at executor setup time. Zeph emits a **RESTART REQUIRED** warning when you change this setting

You do not need to restart Zeph when modifying `blocked_commands` — the agent picks up the new blocklist immediately. If you modify `allowed_commands`, you must restart Zeph for the change to take effect.

### Plugin Security

- **Path traversal defense**: skill paths in the manifest are canonicalized and must resolve within the plugin root directory
- **Config overlay validation**: only `tools.blocked_commands`, `tools.allowed_commands`, and `skills.disambiguation_threshold` are permitted; other keys are rejected
- **Trust escalation filter**: bundled skills are assigned the `Trusted` trust level automatically at startup, bypassing the default `quarantined` level that external skills receive

See [Skill Trust Levels](../advanced/skill-trust.md) for how trust levels control tool access.

## Agent-Invocable Skills

Skills are typically matched to user queries automatically via semantic embedding. With the `invoke_skill` tool, the agent can explicitly fetch and execute any registered skill by name at runtime. This is useful for:

- Skills that should only run when explicitly requested
- Composing multiple skills in a single response
- Overriding the default embedding-based matching

### Using invoke_skill in the LLM Response

When the agent needs to reference or use a skill, it calls the `invoke_skill` tool:

```
I'll use the "git-workflow" skill to help you:
<invoke_skill>
{
  "skill_name": "git-workflow",
  "args": "--verbose"
}
</invoke_skill>
```

The tool returns the skill body with security-aware sanitization:
- **Blocked skills**: refused with an error message
- **Trusted skills**: body returned as-is
- **Quarantined skills**: body wrapped with a quarantine warning

### CLI Usage

Invoke skills from the command line:

```bash
zeph skill invoke git-workflow --verbose
zeph skill invoke deploy-prod --environment staging
```

### Catalog

The agent sees an `invoke_skill` catalog during context assembly that lists all available skills with their names and descriptions. Use `/skills` in TUI or CLI to see the full registry.

## Generate a Skill from a Description

Instead of writing SKILL.md manually, use `/skill create` with a natural language description:

```
/skill create "A skill that manages systemd services — start, stop, restart, status"
```

Zeph generates a complete SKILL.md with frontmatter, instructions, and examples. The skill is saved to your skills directory and hot-reloaded immediately. Duplicate detection prevents creating skills that overlap with existing ones.

See [NL Skill Generation](../advanced/nl-skill-generation.md) for details on generation from descriptions and GitHub repository mining.

## Next Steps

- [Skills](../concepts/skills.md) — how embedding-based matching works
- [Self-Learning Skills](../advanced/self-learning.md) — automatic skill evolution
- [NL Skill Generation](../advanced/nl-skill-generation.md) — generate skills from descriptions or repos
- [Skill Trust Levels](../advanced/skill-trust.md) — security model for imported skills
