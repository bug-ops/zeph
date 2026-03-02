# Instruction Files

Zeph automatically loads project-specific instruction files from the working directory and injects their content into the system prompt before every inference call. This lets you give the agent standing context — coding conventions, domain knowledge, project rules — without repeating them in every message.

## How it works

At startup, Zeph scans the working directory for instruction files and loads them into memory. The content is injected into the **volatile section** of the system prompt (Block 2), after environment context and before skills and tool catalog. This placement keeps the stable cache block (Block 1) intact for prompt caching.

Each loaded file appears as:

```
<!-- instructions: CLAUDE.md -->
<file content>
```

Only the filename (not the full path) is embedded in the prompt.

## File discovery

Files are loaded in the following order:

| Priority | Path | Condition |
|----------|------|-----------|
| 1 | `zeph.md` | Always (any provider) |
| 2 | `.zeph/zeph.md` | Always (any provider) |
| 3 | `CLAUDE.md` | Provider: `claude` |
| 4 | `.claude/CLAUDE.md` | Provider: `claude` |
| 5 | `.claude/rules/*.md` | Provider: `claude` (sorted by name) |
| 6 | `AGENTS.override.md` | Provider: `openai` |
| 7 | `AGENTS.md` | Provider: `openai`, `ollama`, `compatible`, `candle` |
| 8 | Explicit files | `[agent.instructions] extra_files` or `--instruction-file` |

`zeph.md` and `.zeph/zeph.md` are **always** loaded regardless of provider or `auto_detect` setting — they are the universal entry point for project instructions.

## Deduplication

Candidates are deduplicated by canonical path before loading. Symlinks that resolve to the same file are counted once. Files that are already loaded via another candidate path are skipped.

## Security

- **Path traversal protection**: the canonical path of each file must remain within the project root. Symlinks pointing outside the project directory are rejected with a warning.
- **Null byte guard**: files containing null bytes are skipped (indicates binary or corrupted content).
- **Size cap**: files exceeding `max_size_bytes` (default 256 KiB) are skipped. Configurable.
- **No TOCTOU**: a single `File::open()` call is used; metadata and content are read from the same open file handle.

## Configuration

```toml
[agent.instructions]
auto_detect   = true    # Auto-detect provider-specific files (default: true)
extra_files   = []      # Additional files to load (absolute or relative to cwd)
max_size_bytes = 262144  # Per-file size cap, bytes (default: 256 KiB)
```

```bash
# Supply extra instruction files at startup (repeatable)
zeph --instruction-file /path/to/rules.md --instruction-file conventions.md
```

> [!TIP]
> Use `zeph.md` in your project root for rules that apply regardless of which LLM provider you use. Use `CLAUDE.md` or `AGENTS.md` alongside it for provider-specific overrides.

> [!NOTE]
> Instruction files are loaded once at startup and held in memory. Editing a file while the agent is running has no effect until restart. See [issue #1124](https://github.com/bug-ops/zeph/issues/1124) for planned hot-reload support.

## Example: `zeph.md`

```markdown
# Project Instructions

- Language: TypeScript, strict mode
- Test framework: vitest
- Commit messages follow Conventional Commits
- Never modify files under `generated/`
- Prefer explicit type annotations over inference
```

Place this file in your project root. Zeph will include it in every system prompt automatically.
