# Skills

Skills give Zeph specialized knowledge for specific tasks. Each skill is a markdown file (`SKILL.md`) containing instructions and examples that are injected into the LLM prompt when relevant.

Instead of loading all skills into every prompt, Zeph selects only the top-K most relevant (default: 5) via embedding similarity. This keeps prompt size constant regardless of how many skills are installed.

## How Matching Works

1. You send a message — for example, "check disk usage on this server"
2. Zeph embeds your query using the configured embedding model
3. The top 5 most relevant skills are selected by cosine similarity
4. Selected skills are injected into the system prompt
5. Zeph responds using the matched skills

This happens automatically on every message. You never activate skills manually.

## Bundled Skills

| Skill | Description |
|-------|-------------|
| `api-request` | HTTP API requests using curl |
| `docker` | Docker container operations |
| `file-ops` | File system operations — list, search, read, analyze |
| `git` | Git version control — status, log, diff, commit, branch |
| `mcp-generate` | Generate MCP-to-skill bridges |
| `setup-guide` | Configuration reference |
| `skill-audit` | Spec compliance and security review |
| `skill-creator` | Create new skills |
| `system-info` | System diagnostics — OS, disk, memory, processes |
| `web-scrape` | Extract data from web pages |
| `web-search` | Search the internet |

Use `/skills` in chat to see active skills and their usage statistics.

## Key Properties

- **Progressive loading**: only metadata (~100 tokens per skill) is loaded at startup. Full body is loaded on first activation and cached
- **Hot-reload**: edit a `SKILL.md` file, changes apply without restart
- **Two matching backends**: in-memory (default) or Qdrant (faster startup with many skills, delta sync via BLAKE3 hash)
- **Secret gating**: skills that declare `requires-secrets` in their frontmatter are excluded from the prompt if the required secrets are not present in the vault. This prevents the agent from attempting to use a skill that would fail due to missing credentials

## External Skill Management

Zeph includes a `SkillManager` that installs, removes, and verifies external skills. Skills can be installed from git URLs or local paths into the managed directory (`~/.config/zeph/skills/`), which is automatically appended to `skills.paths`.

Installed skills start at the `quarantined` trust level. Use `zeph skill verify` to check BLAKE3 integrity, then promote with `zeph skill trust <name> verified` or `zeph skill trust <name> trusted`.

See [CLI Reference — `zeph skill`](../reference/cli.md#zeph-skill) for the full subcommand list, or use the in-session `/skill install` and `/skill remove` commands for hot-reloaded management without restart.

## Deep Dives

- [Add Custom Skills](../guides/custom-skills.md) — create your own skills
- [Self-Learning Skills](../advanced/self-learning.md) — how skills evolve through failure detection
- [Skill Trust Levels](../advanced/skill-trust.md) — security model for imported skills
