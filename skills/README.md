# Skills

Skills are contextual instructions that Zeph loads automatically based on what you ask. Instead of having all capabilities active at once, Zeph picks the most relevant skills for each conversation turn and uses them to give you better answers.

## How It Works

1. **You send a message** — for example, "check disk usage on this server"
2. **Zeph embeds your query** using the configured embedding model
3. **Top matching skills are selected** — by default, the 5 most relevant ones ranked by vector similarity
4. **Selected skills are injected** into the system prompt, giving Zeph specific instructions and examples for the task
5. **Zeph responds** using the knowledge from matched skills

This happens automatically on every message. You don't need to activate skills manually.

### Matching Backends

Zeph supports two skill matching backends:

- **In-memory** (default) — embeddings are computed on startup and matched via cosine similarity. No external dependencies required.
- **Qdrant** — when semantic memory is enabled and Qdrant is reachable, skill embeddings are persisted in a `zeph_skills` collection. On startup, only changed skills are re-embedded using BLAKE3 content hash comparison. Qdrant's HNSW index handles the vector search. If Qdrant becomes unavailable, Zeph falls back to in-memory matching automatically.

> [!TIP]
> The Qdrant backend significantly reduces startup time when you have many skills, since unchanged skills skip the embedding step entirely.

> [!TIP]
> Use `/skills` in chat to see all available skills and their usage statistics.

## Bundled Skills

| Skill | Description |
|-------|-------------|
| `api-request` | HTTP API requests using curl — GET, POST, PUT, DELETE with headers and JSON |
| `docker` | Docker container operations — build, run, ps, logs, compose |
| `file-ops` | File system operations — list, search, read, and analyze files |
| `git` | Git version control — status, log, diff, commit, branch management |
| `setup-guide` | Configuration reference — LLM providers, memory, tools, and operating modes |
| `system-info` | System diagnostics — OS, disk, memory, processes, uptime |
| `web-scrape` | Extract structured data from web pages using CSS selectors |
| `web-search` | Search the internet for current information |

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

- `name` — unique identifier for the skill
- `description` — used for matching against user queries. Write it so that the embedding model can connect user intent to this skill. Be specific: "Extract structured data from web pages using CSS selectors" works better than "Web stuff"

**Body:** markdown with instructions, code examples, or reference material. This is injected verbatim into the LLM context when the skill is selected.

### Example: Custom Deployment Skill

```markdown
---
name: deploy
description: Deploy application to production using SSH and rsync.
---
# Deploy

## Sync files to server
\```bash
rsync -avz --exclude '.git' ./ user@server:/app/
\```

## Restart service
\```bash
ssh user@server 'sudo systemctl restart myapp'
\```
```

> [!IMPORTANT]
> The `description` field is critical for skill matching. If your skill isn't being selected, try rewriting the description with keywords that match how users would phrase their requests.

## Configuration

### Skill Paths

By default, Zeph scans `./skills` for skill directories. Add more paths in `config/default.toml`:

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

### Embedding Model

Skill matching uses the configured embedding model (default: `qwen3-embedding` via Ollama). Make sure the model is pulled:

```bash
ollama pull qwen3-embedding
```

If the embedding model is unavailable, Zeph falls back to using all skills for every query.

## Hot Reload

Zeph watches skill directories for changes. When you edit, add, or remove a `SKILL.md` file, skills are automatically reloaded without restarting the agent. Changes take effect on the next message.

With the Qdrant backend, hot-reload triggers a delta sync — only modified skills are re-embedded and updated in the collection. Orphan points (removed skills) are cleaned up automatically.
