# Skills

Zeph uses an embedding-based skill system that dramatically reduces token consumption: instead of injecting all skills into every prompt, only the top-K most relevant (default: 5) are selected per query via cosine similarity of vector embeddings. Combined with progressive loading (metadata at startup, bodies on activation, resources on demand), this keeps prompt size constant regardless of how many skills are installed.

## How It Works

1. **You send a message** — for example, "check disk usage on this server"
2. **Zeph embeds your query** using the configured embedding model
3. **Top matching skills are selected** — by default, the 5 most relevant ones ranked by vector similarity
4. **Selected skills are injected** into the system prompt, giving Zeph specific instructions and examples for the task
5. **Zeph responds** using the knowledge from matched skills

This happens automatically on every message. You don't need to activate skills manually.

## Matching Backends

Zeph supports two skill matching backends:

- **In-memory** (default) — embeddings are computed on startup and matched via cosine similarity. No external dependencies required.
- **Qdrant** — when semantic memory is enabled and Qdrant is reachable, skill embeddings are persisted in a `zeph_skills` collection. On startup, only changed skills are re-embedded using BLAKE3 content hash comparison. If Qdrant becomes unavailable, Zeph falls back to in-memory matching automatically.

> The Qdrant backend significantly reduces startup time when you have many skills, since unchanged skills skip the embedding step entirely.

## Bundled Skills

| Skill | Description |
|-------|-------------|
| `api-request` | HTTP API requests using curl — GET, POST, PUT, DELETE with headers and JSON |
| `docker` | Docker container operations — build, run, ps, logs, compose |
| `file-ops` | File system operations — list, search, read, and analyze files |
| `git` | Git version control — status, log, diff, commit, branch management |
| `mcp-generate` | Generate MCP-to-skill bridges for external tool servers |
| `setup-guide` | Configuration reference — LLM providers, memory, tools, and operating modes |
| `skill-audit` | Spec compliance and security review of installed skills |
| `skill-creator` | Create new skills following the agentskills.io specification |
| `system-info` | System diagnostics — OS, disk, memory, processes, uptime |
| `web-scrape` | Extract structured data from web pages using CSS selectors |
| `web-search` | Search the internet for current information |

Use `/skills` in chat to see all available skills and their usage statistics.

## Creating Custom Skills

A skill is a single `SKILL.md` file inside a named directory:

```
skills/
└── my-skill/
    └── SKILL.md
```

### SKILL.md Format

Each file has two parts: a YAML header and a markdown body.

```markdown
---
name: my-skill
description: Short description of what this skill does.
---
# My Skill

Instructions and examples go here.
```

**Header fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Unique identifier (1-64 chars, lowercase, hyphens allowed) |
| `description` | Yes | Used for embedding-based matching against user queries |
| `compatibility` | No | Runtime requirements (e.g., "requires curl") |
| `license` | No | Skill license |
| `allowed-tools` | No | Comma-separated tool names this skill can use |
| `metadata` | No | Arbitrary key-value pairs for forward compatibility |

**Body:** markdown with instructions, code examples, or reference material. Injected verbatim into the LLM context when the skill is selected.

### Skill Resources

Skills can include additional resource directories:

```
skills/
└── system-info/
    ├── SKILL.md
    └── references/
        ├── linux.md
        ├── macos.md
        └── windows.md
```

Resources in `scripts/`, `references/`, and `assets/` are loaded on demand with path traversal protection. OS-specific reference files (named `linux.md`, `macos.md`, `windows.md`) are automatically filtered by the current platform.

### Name Validation

Skill names must be 1-64 characters, lowercase letters/numbers/hyphens only, no leading/trailing/consecutive hyphens, and must match the directory name.

## Configuration

### Skill Paths

By default, Zeph scans `./skills` for skill directories. Add more paths in config:

```toml
[skills]
paths = ["./skills", "/home/user/my-skills"]
```

If a skill with the same name appears in multiple paths, the first one found takes priority.

### Max Active Skills

Control how many skills are injected per query:

```toml
[skills]
max_active_skills = 5
```

Or via environment variable:

```bash
export ZEPH_SKILLS_MAX_ACTIVE=5
```

Lower values reduce prompt size but may miss relevant skills. Higher values include more context but use more tokens.

## Progressive Loading

Only metadata (~100 tokens per skill) is loaded at startup for embedding and matching. Full body (<5000 tokens) is loaded lazily on first activation and cached via `OnceLock`. Resource files are loaded on demand.

With 50+ skills installed, a typical prompt still contains only 5 — saving thousands of tokens per request compared to naive full-injection approaches.

## Hot Reload

SKILL.md file changes are detected via filesystem watcher (500ms debounce) and re-embedded without restart. Cached bodies are invalidated on reload.

With the Qdrant backend, hot-reload triggers a delta sync — only modified skills are re-embedded and updated in the collection.
