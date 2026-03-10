# Migrate Config

As Zeph gains new features, the configuration file grows. When you upgrade from an older version, your existing `config.toml` may be missing entire sections. The `migrate-config` command closes that gap: it reads your config, adds every missing parameter as a commented-out block with documentation, and reformats the result.

Existing values are never changed. The command is safe to run multiple times — the output is identical on each run (idempotent).

## Quick Start

Preview what would change without touching your file:

```bash
zeph migrate-config --config ~/.zeph/config.toml --diff
```

Apply the migration in place:

```bash
zeph migrate-config --config ~/.zeph/config.toml --in-place
```

## What It Does

Given a minimal config like:

```toml
[agent]
model = "claude-sonnet-4-6"
```

After migration, missing sections appear as commented-out blocks:

```toml
[agent]
model = "claude-sonnet-4-6"

# [llm]
# # Maximum tokens allowed in a single LLM request.
# max_tokens = 8192
# # Number of retry attempts on transient errors.
# retries = 3
# ...

# [memory]
# # SQLite database path.
# db_path = ".zeph/data/zeph.db"
# ...
```

To activate a section, uncomment the `[section]` header and the parameters you want to change. Delete or leave commented any that you want to keep at their defaults.

## Flags

| Flag | Description |
|------|-------------|
| `--config <PATH>` | Path to the config file to migrate. Defaults to the standard config search path. |
| `--in-place` | Write the migrated output back to the same file atomically. Without this flag, output goes to stdout. |
| `--diff` | Print a unified diff of changes instead of the full file. Useful for reviewing before committing. |

## Typical Workflow

1. Run with `--diff` to review what would be added:

   ```bash
   zeph migrate-config --config config.toml --diff
   ```

2. If the diff looks correct, apply in place:

   ```bash
   zeph migrate-config --config config.toml --in-place
   ```

3. Open the file and uncomment any new parameters you want to configure.

4. Restart Zeph with the updated config.

## What Gets Added

The canonical reference covers all config sections:

- `[agent]` — model, system prompt, token budgets, instruction files
- `[llm]` — provider-level timeouts, retries, streaming
- `[memory]` — SQLite path, session limits, compaction, decay, MMR
- `[tools]` — shell sandbox, web scrape, filters, audit, anomaly detection
- `[channels]` — Telegram, Discord, Slack settings
- `[tui]` — TUI dashboard display options
- `[mcp]` — MCP server definitions
- `[a2a]` — A2A protocol settings
- `[acp]` — Agent Client Protocol (stdio/HTTP/WebSocket)
- `[agents]` — sub-agent concurrency and memory scope defaults
- `[orchestration]` — task graph and planner settings
- `[graph-memory]` — entity extraction and knowledge graph options
- `[security]` — content isolation, exfiltration guard, quarantine
- `[vault]` — secrets backend (env or age)
- `[scheduler]` — cron task scheduler
- `[gateway]` — HTTP webhook ingestion
- `[index]` — AST-based code indexing
- `[experiments]` — A/B testing for prompt parameters
- `[logging]` — log level, file output, rotation

Parameters that already exist in your file are never overwritten or reordered within their section.

## TUI Usage

In an interactive session, run:

```
> /migrate-config
```

or open the command palette and select **config:migrate**. The TUI shows the diff as a system message. To apply changes, use the CLI `--in-place` flag.

## Notes

- The reference config is embedded in the binary — no network access or external files required.
- Unknown keys you have added to your config are preserved at the end of each section.
- Array-of-tables blocks (`[[compatible]]`, `[[mcp.servers]]`) are passed through unchanged.
- The `--in-place` write is atomic: the file is written to a temporary location in the same directory and renamed, so a crash mid-write cannot corrupt the original.
