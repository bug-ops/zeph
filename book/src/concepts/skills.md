# Skills

Skills give Zeph specialized knowledge for specific tasks. Each skill is a markdown file (`SKILL.md`) containing instructions and examples that are injected into the LLM prompt when relevant.

Instead of loading all skills into every prompt, Zeph selects only the top-K most relevant (default: 5) using a combination of BM25 keyword matching and embedding cosine similarity fused via Reciprocal Rank Fusion. This keeps prompt size constant regardless of how many skills are installed.

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
- **Two matching backends**: in-memory (default) or Qdrant (faster startup with many skills, delta sync via BLAKE3 hash). Both support BM25+cosine hybrid search via Reciprocal Rank Fusion (enabled by default, disable with `hybrid_search = false`)
- **Secret gating**: skills that declare `x-requires-secrets` in their frontmatter are excluded from the prompt if the required secrets are not present in the vault. This prevents the agent from attempting to use a skill that would fail due to missing credentials
- **Compact prompt mode**: when context budget is tight, `skills.prompt_mode = "auto"` (default) switches to a condensed XML format that includes only name, description, and triggers — ~80% smaller than full bodies. Force with `"compact"` or disable with `"full"`. See [Context Engineering — Skill Prompt Modes](../advanced/context.md#skill-prompt-modes)
- **Channel allowlist**: skills can declare which I/O channels they are permitted to run on via `x-channels` in YAML frontmatter. When set, the skill is excluded from matching on channels not in the list. Omit to allow all channels.
- **Description cap**: skill descriptions are capped at 2048 characters to prevent oversized prompt injection from user-created skills
- **Injection sanitization**: skill bodies and `/skill create` inputs are sanitized against prompt injection. URL domains in skill bodies are checked against a configurable allowlist. Untrusted skill content has structural XML tags escaped before prompt injection

## Natural Language Skill Generation

Use `/skill create` to generate a new skill from a natural language description:

```
/skill create "A skill that formats JSON files using jq"
```

Zeph generates a complete `SKILL.md` with frontmatter, instructions, and examples via LLM reflection. Skills can also be mined from GitHub repositories — Zeph analyzes repo structure and README to extract actionable skill definitions.

Duplicate detection prevents creating skills that overlap with existing ones by checking semantic similarity against the skill registry.

## Semantic Confusability Mitigation

When multiple skills have overlapping descriptions, the matcher can confuse them. Zeph mitigates this with:

- **Category grouping**: skills are grouped by functional category, and matching considers category affinity alongside raw similarity
- **Two-stage matching**: an initial broad match is followed by a disambiguation stage that compares top candidates within the same category
- Use `/skill confusability` to generate a report showing which skills are at risk of being confused

## External Skill Management

Zeph includes a `SkillManager` that installs, removes, and verifies external skills. Skills can be installed from git URLs or local paths into the managed directory (`~/.config/zeph/skills/`), which is automatically appended to `skills.paths`.

Installed skills start at the `quarantined` trust level. Use `zeph skill verify` to check BLAKE3 integrity, then promote with `zeph skill trust <name> verified` or `zeph skill trust <name> trusted`.

See [CLI Reference — `zeph skill`](../reference/cli.md#zeph-skill) for the full subcommand list, or use the in-session `/skill install` and `/skill remove` commands for hot-reloaded management without restart.

## Deep Dives

- [Add Custom Skills](../guides/custom-skills.md) — create your own skills
- [Self-Learning Skills](../advanced/self-learning.md) — how skills evolve through failure detection
- [Skill Trust Levels](../advanced/skill-trust.md) — security model for imported skills
