# CLI Reference

Zeph uses [clap](https://docs.rs/clap) for argument parsing. Run `zeph --help` for the full synopsis.

## Usage

```
zeph [OPTIONS] [COMMAND]
```

## Subcommands

| Command | Description |
|---------|-------------|
| `init`  | Interactive configuration wizard (see [Configuration Wizard](../getting-started/wizard.md)) |
| `agents` | Manage sub-agent definitions — list, show, create, edit, delete (see [Sub-Agent Orchestration](../advanced/sub-agents.md#managing-definitions)) |
| `skill` | Manage external skills — install, remove, verify, trust (see [Skill Trust Levels](../advanced/skill-trust.md)) |
| `memory` | Export and import conversation history snapshots |
| `project` | Project-level management — purge all local state (see below) |
| `vault` | Manage the age-encrypted secrets vault (see [Secrets Management](security.md#age-vault)) |
| `router` | Inspect or reset Thompson Sampling router state (see [Adaptive Inference](../advanced/adaptive-inference.md)) |
| `ingest` | Ingest a document or directory into semantic memory (Qdrant collection) |
| `classifiers` | Manage ML classifier models — list, download, status |
| `sessions` | Manage ACP session history — list, show, delete (requires `acp` feature) |
| `schedule` | Manage cron-based scheduled jobs — list, add, remove, show (requires `scheduler` feature; see [Scheduler](../concepts/scheduler.md)) |
| `db` | Database management — run migrations, check status (see [Database Abstraction](../concepts/database.md)) |
| `durable` | Inspect the durable execution journal — list, show, inspect, prune, resume (see [Durable Journal Encryption](security/durable-encryption.md)) |
| `migrate-config` | Add missing config parameters as commented-out blocks and reformat the file (see [Migrate Config](../guides/migrate-config.md)) |
| `worktree` | Manage background sub-agent git worktrees — list active, remove stale (requires `[worktree] enabled = true`; see [Worktree Isolation](../guides/worktree.md)) |

When no subcommand is given, Zeph starts the agent loop.

### `zeph db`

Manage database schema migrations.

| Subcommand | Description |
|------------|-------------|
| `db migrate` | Apply pending database migrations |
| `db migrate --status` | Show migration status without applying changes |

```bash
zeph db migrate                    # apply pending migrations
zeph db migrate --status           # check what would be applied
```

### `zeph durable`

Inspect the durable execution journal directly — no running agent process is
required. Output is **redacted by default** (INV-5): payload bytes and resolver
tokens are shown only with `--reveal`, which decrypts through the vault-resolved
`ZEPH_DURABLE_KEY`.

| Subcommand | Description |
|------------|-------------|
| `durable list [--status <s>] [--kind <k>] [--limit <n>]` | List executions, newest first |
| `durable show <id> [--reveal]` | Show an execution's journal entries (metadata only by default) |
| `durable inspect <id> --step <n> [--reveal]` | Inspect a single step entry |
| `durable prune [--dry-run]` | Sweep terminal executions past their TTL |
| `durable resume <id>` | Report resume state for an execution |

```bash
zeph durable list --status running          # in-flight executions
zeph durable show <uuid>                     # redacted journal entries
zeph durable show <uuid> --reveal            # decrypted payloads (prints a warning)
zeph durable prune --dry-run                 # how many would be pruned
```

### `zeph init`

Generate a `config.toml` through a guided wizard.

```bash
zeph init                          # write to ./config.toml (default)
zeph init --output ~/.zeph/config.toml  # specify output path
```

Options:

| Flag | Short | Description |
|------|-------|-------------|
| `--output <PATH>` | `-o` | Output path for the generated config file |

### `zeph skill`

Manage external skills. Installed skills are stored in `~/.config/zeph/skills/`.

| Subcommand | Description |
|------------|-------------|
| `skill install <url\|path>` | Install a skill from a git URL or local directory path |
| `skill remove <name>` | Remove an installed skill by name |
| `skill list` | List installed skills with trust level and source metadata |
| `skill verify [name]` | Verify BLAKE3 integrity of one or all installed skills |
| `skill trust <name> [level]` | Show or set trust level (`trusted`, `verified`, `quarantined`, `blocked`). Promoting to `trusted`/`verified` arms the per-invocation BLAKE3 integrity re-check by default (`[skills.trust] require_integrity_check_on_promote`); add `--require-check`/`--no-require-check` to force it on/off (see [Skill Trust Levels](../advanced/skill-trust.md#per-invocation-integrity-re-check)) |
| `skill block <name>` | Block a skill (deny all tool access) |
| `skill unblock <name>` | Unblock a skill (revert to `quarantined`) |
| `skill promote-heuristics [--skill <name>]` | Dry-run: show skills eligible for A6 heuristic → full promotion (requires `[skills.learning.heuristic_promotion_enabled = true]`) |
| `skill search <query>` | Search the configured external registry by keyword (requires `[skills.registry] enabled = true`) |
| `skill get <registry-id>` | Install a skill by registry ID returned by `skill search` (requires `[skills.registry] enabled = true`) |

```bash
# Install from git
zeph skill install https://github.com/user/zeph-skill-example.git

# Install from local path
zeph skill install /path/to/my-skill

# List installed skills
zeph skill list

# Verify integrity and promote trust (arms the per-invocation integrity re-check by default)
zeph skill verify my-skill
zeph skill trust my-skill trusted

# Promote without arming the per-invocation integrity re-check
zeph skill trust my-skill trusted --no-require-check

# Remove a skill
zeph skill remove my-skill

# Show skills eligible for heuristic promotion (dry-run)
zeph skill promote-heuristics

# Show eligibility for a specific skill
zeph skill promote-heuristics --skill my-skill

# Search the external registry for skills (requires registry enabled in config)
zeph skill search "json parsing"

# Install a skill by registry ID
zeph skill get skills-sh/json-formatter

# View registry configuration
zeph init                              # or use --migrate-config to add the [skills.registry] section
```

### `zeph plugin`

Manage plugin packages (collections of skills, MCP servers, and config overlays). Installed plugins are stored in `~/.local/share/zeph/plugins/`.

| Subcommand | Description |
|------------|-------------|
| `plugin list` | List installed plugins with installation timestamps |
| `plugin list --overlay` | Show which plugins are active and which were skipped (with reasons), including integrity check failures |
| `plugin add <path>` | Install a plugin from a local directory path (must contain `plugin.toml`) |
| `plugin remove <name>` | Remove an installed plugin by name |
| `plugin disable <name> [--force]` | Disable a plugin (optional `--force` to skip confirmation and enforcement checks) |
| `plugin search <query>` | Search the configured external registry by keyword (requires `[skills.registry] enabled = true`) |
| `plugin get <registry-id>` | Install a plugin by registry ID returned by `plugin search` (requires `[skills.registry] enabled = true`) |

```bash
# List installed plugins
zeph plugin list

# Show the active plugin overlay (useful for diagnosing load failures)
zeph plugin list --overlay

# Install a plugin from a local directory
zeph plugin add /path/to/my-plugin

# Remove a plugin
zeph plugin remove my-plugin

# Disable a plugin (with confirmation)
zeph plugin disable my-plugin

# Force-disable a plugin (skip confirmation)
zeph plugin disable my-plugin --force

# Search the external registry for plugins (requires registry enabled in config)
zeph plugin search "github-tools"

# Install a plugin by registry ID
zeph plugin get skills-sh/github-integration

# View registry configuration
zeph init                              # or use --migrate-config to add the [skills.registry] section
```

**Overlay flag note:** `--overlay` shows which plugins contributed to the active config and which were skipped (with reasons like "integrity mismatch", "invalid manifest", etc.). This is evaluated against the default config — use `--config <path>` in the agent to see the live intersection with your active config.

**Integrity checks:** When you install a plugin, Zeph records a sha256 digest of its `.plugin.toml`. At startup and hot-reload, the digest is verified. If it doesn't match, the plugin is skipped and the mismatch is visible in `plugin list --overlay`. See [Plugin Manifest Integrity](security.md#plugin-manifest-integrity) for details.

**Ephemeral plugins (session-scoped):** Use the global `--plugin-url` flag to load plugins for a single session without permanent installation:

```bash
# Load a plugin from a remote URL (HTTPS only)
zeph --plugin-url https://example.com/my-plugin.tar.gz

# Multiple plugins
zeph --plugin-url https://example.com/plugin1.tar.gz --plugin-url https://example.com/plugin2.tar.gz

# Pin a plugin version using url@sha256 syntax
zeph --plugin-url https://example.com/plugin.tar.gz@abc123def456789

# Combine ephemeral and permanent plugins
zeph --plugin-url https://example.com/plugin.tar.gz
```

Ephemeral plugins are scanned for security issues before loading and removed when the session ends. They cannot be disabled or permanently installed; use `zeph plugin add` for persistent plugins.

### `zeph memory`

Manage conversation history and advanced memory subsystems.

| Subcommand | Description |
|------------|-------------|
| `memory export <path>` | Export all conversations, messages, and summaries to a JSON file |
| `memory import <path>` | Import a snapshot file into the local database (duplicates are skipped) |
| `memory trajectory` | List trajectory memory entries (procedural and episodic) for the current conversation (requires `[memory.trajectory] enabled = true`) |
| `memory tree` | Show TiMem memory tree nodes and consolidation statistics (requires `[memory.tree] enabled = true`) |

```bash
# Back up all conversation data
zeph memory export backup.json

# Restore on another machine
zeph memory import backup.json

# Inspect trajectory entries
zeph memory trajectory

# Inspect memory tree state
zeph memory tree
```

The snapshot format is versioned (currently v1). Import uses `INSERT OR IGNORE` — re-importing the same file is safe and skips existing records.

### `zeph project`

Manage project-level state and cleanup.

| Subcommand | Description |
|------------|-------------|
| `project purge` | Remove all project-local state (database, logs, debug artifacts, Qdrant collections) with safety checks |

**`zeph project purge` options:**

| Flag | Short | Description |
|------|-------|-------------|
| `--config <PATH>` | `-c` | Path to config file (defaults to standard search path) |
| `--dry-run` | | Show what would be removed without deleting anything |
| `--yes` | `-y` | Skip confirmation prompt (database lock check is never skipped) |

**Removes:**

- SQLite database file (`zeph.db`) and its siblings (`zeph.db-wal`, `zeph.db-shm`)
- Main log file and any rotated log files
- Scheduler daemon log and PID file
- Debug dump artifacts directory
- Trace files directory
- Audit log file (if configured as a file path)
- All 10 known Qdrant collections (when `vector_backend = "qdrant"`)

**Safety:**

- Pre-flight exclusive lock check on the SQLite database — aborts immediately if an agent session is running
- Database lock check is always enforced, even with `-y`
- Respects vector backend configuration: skips Qdrant when `vector_backend = "sqlite"`
- Respects database configuration: skips SQLite file deletion when using PostgreSQL

```bash
# Preview what would be removed
zeph project purge --dry-run

# Remove all project state (after confirmation)
zeph project purge

# Remove without confirmation (but DB lock check still applies)
zeph project purge -y

# Use a custom config path
zeph project purge --config ~/.zeph/custom-config.toml --yes
```

> [!WARNING]
> `zeph project purge` is destructive. This action cannot be undone. Ensure you have backups if you need to preserve any state.

> [!TIP]
> Use `--dry-run` first to see the byte counts that would be deleted. This helps you estimate storage recovery and verify the correct state will be removed.

### `zeph agents`

Manage sub-agent definition files. See [Managing Definitions](../advanced/sub-agents.md#managing-definitions) for examples and field details.

| Subcommand | Description |
|------------|-------------|
| `agents list` | List all loaded definitions with scope, model, and description |
| `agents show <name>` | Print details for a single definition |
| `agents create <name> -d <desc>` | Create a new definition stub in `.zeph/agents/` |
| `agents edit <name>` | Open the definition in `$VISUAL` / `$EDITOR` and re-validate on save |
| `agents delete <name>` | Delete a definition file (prompts for confirmation) |

```bash
# List all definitions (project and user scope)
zeph agents list

# Inspect a single definition
zeph agents show code-reviewer

# Create a project-scoped definition
zeph agents create reviewer --description "Code review helper"

# Create a user-scoped (global) definition
zeph agents create helper --description "General helper" --dir ~/.config/zeph/agents/

# Edit with $EDITOR
zeph agents edit reviewer

# Delete without confirmation prompt
zeph agents delete reviewer --yes
```

### `zeph vault`

Manage age-encrypted secrets without manual `age` CLI invocations.

| Subcommand | Description |
|------------|-------------|
| `vault init` | Generate an age keypair and empty encrypted vault |
| `vault set <KEY> <VALUE>` | Encrypt and store a secret |
| `vault get <KEY>` | Decrypt and print a secret value |
| `vault list` | List stored secret keys (values are not printed) |
| `vault rm <KEY>` | Remove a secret from the vault |

Default paths (created by `vault init`):

- Key file: `~/.config/zeph/vault-key.txt`
- Vault file: `~/.config/zeph/secrets.age`

Override with `--vault-key` and `--vault-path` global flags.

```bash
zeph vault init
zeph vault set ZEPH_CLAUDE_API_KEY sk-ant-...
zeph vault set ZEPH_TELEGRAM_TOKEN 123:ABC
zeph vault list
zeph vault get ZEPH_CLAUDE_API_KEY
zeph vault rm ZEPH_TELEGRAM_TOKEN
```

### `zeph migrate-config`

Update an existing config file with all parameters added since it was last generated. Missing sections are appended as commented-out blocks with documentation. Existing values are never modified.

| Flag | Short | Description |
|------|-------|-------------|
| `--config <PATH>` | `-c` | Path to the config file (defaults to standard search path) |
| `--in-place` | | Write result back to the same file atomically |
| `--diff` | | Print a unified diff to stdout instead of the full file |

```bash
# Preview what would be added
zeph migrate-config --config config.toml --diff

# Apply in place
zeph migrate-config --config config.toml --in-place

# Print migrated config to stdout
zeph migrate-config --config config.toml
```

See [Migrate Config](../guides/migrate-config.md) for a full walkthrough.

### `zeph worktree`

Manage background sub-agent git worktrees for isolation. Requires `[worktree] enabled = true` in the config. See [Worktree Isolation](../guides/worktree.md) for details.

| Subcommand | Description |
|------------|-------------|
| `worktree list` | List all active worktrees managed by Zeph with paths and creation timestamps |
| `worktree clean` | Remove stale worktrees (those no longer tracked by active sub-agents) |

```bash
# List all active worktrees
zeph worktree list

# Clean up unused worktrees (safe operation, only removes untracked ones)
zeph worktree clean
```

Each sub-agent spawned with background isolation gets its own git worktree cloned from your repository. The `list` command shows which worktrees are active; `clean` removes ones that are no longer in use.

### `zeph router`

Inspect or reset the Thompson Sampling router state file.

| Subcommand | Description |
|------------|-------------|
| `router stats` | Show alpha/beta and mean success rate per provider |
| `router reset` | Delete the state file (resets to uniform priors) |

Both subcommands accept `--state-path <PATH>` to override the default location (`~/.zeph/router_thompson_state.json`).

```bash
zeph router stats
zeph router reset
zeph router stats --state-path /custom/path.json
```

### `zeph schedule`

Manage cron-based scheduled jobs from the command line. Requires the `scheduler` feature. All commands read the same SQLite database used by the running agent.

| Subcommand | Description |
|------------|-------------|
| `schedule list` | List all active scheduled jobs with NAME, KIND, MODE, NEXT RUN, and CRON columns |
| `schedule add <CRON> <PROMPT>` | Add a new periodic job with a cron expression and task prompt |
| `schedule remove <NAME>` | Remove a scheduled job by name |
| `schedule show <NAME>` | Show full details for a single job |

```bash
# List all scheduled jobs
zeph schedule list

# Add a daily cleanup job at 03:00 UTC
zeph schedule add "0 3 * * *" "run memory cleanup"

# Add with an explicit name and task kind
zeph schedule add "0 3 * * *" "run memory cleanup" --name daily-cleanup --kind memory_cleanup

# Show details of a job
zeph schedule show daily-cleanup

# Remove a job
zeph schedule remove daily-cleanup
```

`schedule add` options:

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Job name (auto-generated from prompt hash if omitted) |
| `--kind <KIND>` | Task kind string (default: `custom`) |

See [Scheduler](../concepts/scheduler.md) for the full list of built-in task kinds, cron expression formats, and how jobs are persisted.

### `zeph ingest`

Ingest a document or directory of documents into semantic memory. Chunks the content and stores embeddings in the configured Qdrant collection.

```bash
# Ingest a single file
zeph ingest path/to/doc.md

# Ingest a directory with custom chunk settings
zeph ingest ./docs --chunk-size 500 --chunk-overlap 50 --collection my_docs
```

| Flag | Default | Description |
|------|---------|-------------|
| `--chunk-size <N>` | `1000` | Chunk size in characters |
| `--chunk-overlap <N>` | `100` | Overlap between adjacent chunks in characters |
| `--collection <NAME>` | `zeph_documents` | Target Qdrant collection name |

### `zeph classifiers`

Manage ML classifier model weights. Requires the `classifiers` feature.

| Subcommand | Description |
|------------|-------------|
| `classifiers download` | Pre-download configured model weights to the HuggingFace Hub cache |

```bash
# Download all configured classifier models
zeph classifiers download

# Download only the prompt-injection classifier
zeph classifiers download --model injection

# Download a specific HuggingFace repo
zeph classifiers download --repo protectai/deberta-v3-base-prompt-injection-v2

# Increase download timeout (default: 600 seconds)
zeph classifiers download --timeout-secs 1200
```

`classifiers download` options:

| Flag | Default | Description |
|------|---------|-------------|
| `--model <TYPE>` | `all` | Which model to download: `injection`, `pii`, or `all` |
| `--repo <REPO_ID>` | from config | HuggingFace repo ID override |
| `--timeout-secs <N>` | `600` | Download timeout in seconds |

Model files are cached in `~/.cache/huggingface/hub/`. Run this before starting the agent to avoid slow first-inference downloads.

### `zeph sessions`

Manage durable conversation-session event logs (see [Session Persistence and
Resume](../advanced/session-persistence.md)). Requires the `session` or `acp` feature.

| Subcommand | Description |
|------------|-------------|
| `sessions list` | List sessions — ID, title, status, event count, last updated |
| `sessions show <ID> [--events]` | Session metadata, or the full event-log dump with `--events` |
| `sessions resume <ID>` | Launch a live interactive agent bound to `<ID>`, replaying its history |
| `sessions resume <ID> --print` | Print all events from the session to stdout instead (no agent started; matches the pre-session-persistence `resume` behavior) |
| `sessions fork <ID> [--at N]` | Eager-copy the session into a fresh one, optionally cut at event `N` |
| `sessions export <ID> <path>` | Write the session's event log to a file |
| `sessions import <path>` | Load an exported event log as a new session |
| `sessions delete <ID>` | Delete a session and its event log |

```bash
zeph sessions list
zeph sessions show abc123 --events
zeph sessions resume abc123               # live interactive agent
zeph sessions resume abc123 --print       # dump events, no agent
zeph sessions fork abc123 --at 40
zeph sessions export abc123 backup.jsonl
zeph sessions import backup.jsonl
zeph sessions delete abc123
```

### `zeph serve-sessions`

Run Zeph as a long-lived process exposing durable sessions over HTTP/SSE. See [zeph serve —
Persistent Agent Service](../advanced/serve-mode.md) for the full REST API. Requires the
`session` feature.

```bash
zeph serve-sessions [--http-addr ADDR] [--max-sessions N]
```

`--acp` is not supported here — it errors with guidance to run `zeph --acp` (or `--acp-http`) as
a separate process instead of combining transports in one process.

## Interactive Commands

The following `/`-prefixed commands are available during an interactive session:

### `/agent`

Manage sub-agents. See [Sub-Agent Orchestration](../advanced/sub-agents.md) for details.

| Subcommand | Description |
|------------|-------------|
| `/agent list` | Show available sub-agent definitions |
| `/agent spawn <name> <prompt>` | Start a sub-agent with a task |
| `/agent bg <name> <prompt>` | Alias for `spawn` |
| `/agent status` | Show active sub-agents with state and progress |
| `/agent cancel <id>` | Cancel a running sub-agent (accepts ID prefix) |
| `/agent resume <id> <prompt>` | Resume a completed sub-agent from its transcript |
| `/agent approve <id>` | Approve a pending secret request |
| `/agent deny <id>` | Deny a pending secret request |

```bash
> /agent list
> /agent spawn code-reviewer Review the auth module
> /agent status
> /agent cancel a1b2
> /agent resume a1b2 Fix the remaining warnings
> @code-reviewer Review the auth module   # shorthand for /agent spawn
```

### `/lsp`

Show LSP context injection status. Requires the `lsp-context` feature and mcpls configured under
`[[mcp.servers]]`.

| Usage | Description |
|-------|-------------|
| `/lsp` | Show hook state, MCP server connection status, injection counts per hook type, and current turn token budget usage |

```bash
> /lsp
```

### `/experiment`

Manage experiment sessions. Requires the `experiments` feature. See [Experiments](../concepts/experiments.md) for details.

| Subcommand | Description |
|------------|-------------|
| `/experiment start [N]` | Start a new experiment session. Optional `N` overrides `max_experiments` for this run |
| `/experiment stop` | Cancel the running session (partial results are preserved) |
| `/experiment status` | Show progress of the current session |
| `/experiment report` | Display results from past sessions |
| `/experiment best` | Show the best accepted variation per parameter |

```bash
> /experiment start
> /experiment start 50
> /experiment status
> /experiment stop
> /experiment report
> /experiment best
```

### `/log`

Display the current file logging configuration and recent log entries.

| Usage | Description |
|-------|-------------|
| `/log` | Show log file path, level, rotation, max files, and the last 20 lines |

```bash
> /log
```

See [Logging](../concepts/logging.md) for configuration details.

### `/plugins`

Manage installed plugins interactively. Same operations as the `zeph plugin` CLI command, but available mid-session.

| Subcommand | Description |
|------------|-------------|
| `/plugins list` | List installed plugins with installation timestamps |
| `/plugins list --overlay` | Show the active plugin overlay (which plugins are active/skipped and why) |
| `/plugins overlay` | Alias for `list --overlay` |
| `/plugins add <path>` | Install a plugin from a local directory path |
| `/plugins remove <name>` | Remove an installed plugin by name |

```bash
> /plugins list
> /plugins list --overlay
> /plugins overlay
> /plugins add /path/to/my-plugin
> /plugins remove my-plugin
```

Use `overlay` to diagnose why a plugin didn't load (integrity mismatch, invalid manifest, etc.). This is the same information shown by `zeph plugin list --overlay` in the CLI.

### `/migrate-config`

Show a diff of config changes that `migrate-config` would apply. Opens the command palette entry `config:migrate`.

| Usage | Description |
|-------|-------------|
| `/migrate-config` | Display the migration diff as a system message |

```bash
> /migrate-config
```

To apply changes, use the CLI: `zeph migrate-config --config <path> --in-place`.

See [Migrate Config](../guides/migrate-config.md) for details.

### `/new`

Reset the current conversation while preserving session state (provider, skills, memory backend). Starts a fresh conversation with a new conversation ID without restarting the agent.

```bash
> /new
```

This is useful when you want to change topics without carrying over stale context from a long session.

### `/debug-dump`

Enable debug dump mid-session without restarting.

| Usage | Description |
|-------|-------------|
| `/debug-dump` | Enable dump using the configured `debug.output_dir` |
| `/debug-dump <PATH>` | Enable dump writing to a custom directory |

```bash
> /debug-dump
> /debug-dump /tmp/my-session-debug
```

See [Debug Dump](../advanced/debug-dump.md) for the file layout and how to read dumps.

### `/loop`

Repeat a prompt at fixed intervals. Useful for continuous monitoring, periodic tasks, or testing.

| Subcommand | Description |
|------------|-------------|
| `/loop <PROMPT> every <N> <UNIT>` | Start repeating the prompt every N time units (`seconds`, `minutes`, `hours`) |
| `/loop stop` | Cancel the active loop |
| `/loop status` | Show current loop state |

```bash
> /loop Check for new errors every 30 seconds
> /loop status
> /loop stop
```

Time constraints:
- Minimum interval: 5 seconds
- Prompts starting with `/` are rejected to prevent slash-command injection
- Default max iterations: 1000 (configurable via `[cli.loop] max_iterations`)

### `/recap`

Generate an on-demand summary of the current conversation. Useful for understanding context in long sessions.

| Subcommand | Description |
|------------|-------------|
| `/recap` | Generate and display a session summary |

```bash
> /recap
```

Configuration: Set `[session.recap]` in your config to control which LLM provider and whether to auto-recap on session resume.

### `/cd`

Change the working directory for the agent. This updates the active `cwd` used by tools like `shell` and `read_file`, invalidates the cached repo-map, and re-discovers `CLAUDE.md` and `AGENTS.md` files in the new directory. The system-prompt context block is preserved across the change.

```
/cd <path>
```

Examples:

```bash
> /cd ../sibling-project
> /cd /home/user/workspace/myproject
> /cd .                 # reset to current directory
```

The path must be within the allowed `[tools.shell] allowed_paths` sandbox. Attempting to change to a path outside the sandbox will produce an error.

### `/conv`

Browse, resume, or fork durable conversation-sessions (see [Session Persistence and
Resume](../advanced/session-persistence.md)). Works identically in the CLI, TUI, and chat
channels.

| Subcommand | Description |
|------------|-------------|
| `/conv` or `/conv list` | List sessions — ID, title, status, event count, last updated |
| `/conv show <id>` | One session's metadata |
| `/conv resume <id>` | Live-swap the current conversation onto `<id>`, replaying its history |
| `/conv fork <id>` | Eager-copy `<id>` into a fresh session, then swap onto the copy |

```bash
> /conv list
> /conv show abc123
> /conv resume abc123
> /conv fork abc123
```

## Global Options

| Flag | Description |
|------|-------------|
| `--bare` | Strip the agent to essentials for scripted/CI usage: skips memory initialization, scheduler startup, skill loading, and watcher registration. Faster startup, suitable for piping and non-interactive workflows. Incompatible with `--tui`, `--acp`, and messaging channels |
| `--safe-mode` | Disable project-context, plugins, skills (including hot-reload), hooks, and MCP servers for a single session for troubleshooting. Unlike `--bare` (which is for CI scripting), `--safe-mode` preserves the full agent loop and allows normal interaction — it just strips optional features. Also set via `ZEPH_SAFE_MODE=true` |
| `--json` | Emit structured JSONL events to stdout (boot, chunk, response_end, tool_call, tool_result, cost, error) for programmatic integration. All tool output is redacted. Incompatible with `--tui`, `--acp`, and messaging channels. Tracing redirected to stderr |
| `-y` / `--auto` | Enable full autonomy: skip all tool confirmation prompts. Shell blocklist and adversarial policy enforcement remain active. Use in trusted scripted environments |
| `--tui` | Run with the TUI dashboard (requires the `tui` feature) |
| `--daemon` | Run as headless background agent with A2A endpoint (requires `a2a` feature). See [Daemon Mode](../guides/daemon-mode.md) |
| `--acp` | Run as ACP server over stdio for IDE embedding (requires `acp` feature) |
| `--acp-manifest` | Print ACP agent manifest JSON to stdout and exit (requires `acp` feature) |
| `--acp-http` | Run as ACP server over HTTP+SSE and WebSocket (requires `acp-http` feature) |
| `--acp-http-bind <ADDR>` | Bind address for the ACP HTTP server (requires `acp-http` feature) |
| `--acp-auth-token <TOKEN>` | Bearer token for ACP HTTP/WebSocket auth, overrides `acp.auth_token` (requires `acp-http` feature) |
| `--connect <URL>` | Connect TUI to a remote daemon via A2A SSE streaming (requires `tui` + `a2a` features). See [Daemon Mode](../guides/daemon-mode.md) |
| `--config <PATH>` | Path to a TOML config file (overrides `ZEPH_CONFIG` env var) |
| `--vault <BACKEND>` | Secrets backend: `env` or `age` (overrides `ZEPH_VAULT_BACKEND` env var) |
| `--vault-key <PATH>` | Path to age identity (private key) file (default: `~/.config/zeph/vault-key.txt`, overrides `ZEPH_VAULT_KEY` env var) |
| `--vault-path <PATH>` | Path to age-encrypted secrets file (default: `~/.config/zeph/secrets.age`, overrides `ZEPH_VAULT_PATH` env var) |
| `--thinking <MODE>` | Enable Claude thinking mode: `extended:<budget>`, `adaptive`, or `adaptive:<effort>` (`low`/`medium`/`high`). Overrides config. Example: `--thinking extended:10000` |
| `--reasoning-effort <LEVEL>` | Set the default reasoning-effort level (`low`/`medium`/`high`) at startup for every configured provider that supports it: Claude (adaptive thinking), OpenAI/Compatible (`reasoning_effort`), Gemini (thinking level). No startup equivalent exists for a thinking-token *budget* on non-Claude providers — configure `thinking_budget` in `config.toml` for Gemini, or use the runtime `/think-tokens` command. Can also be changed mid-session with `/reasoning-effort`, session-only |
| `--guardrail` | Enable LLM-based guardrail (prompt injection pre-screening). Overrides `security.guardrail.enabled` |
| `--graph-memory` | Enable graph-based knowledge memory for this session, overriding `memory.graph.enabled`. See [Graph Memory](../concepts/graph-memory.md) |
| `--compression-guidelines` | Enable ACON failure-driven compression guidelines for this session, overriding `memory.compression_guidelines.enabled`. Requires `compression-guidelines` feature at compile time; silently ignored otherwise. See [Memory](../concepts/memory.md) |
| `--lsp-context` | Enable automatic LSP context injection for this session, overriding `agent.lsp.enabled`. Injects diagnostics after file writes and hover info on reads. Requires mcpls MCP server and `lsp-context` feature. See [LSP Code Intelligence](../guides/lsp.md#lsp-context-injection) |
| `--focus` / `--no-focus` | Enable or disable Focus Agent for this session, overriding `agent.focus.enabled` |
| `--sidequest` / `--no-sidequest` | Enable or disable SideQuest eviction for this session, overriding `memory.sidequest.enabled` |
| `--pruning-strategy <STRATEGY>` | Override pruning strategy: `reactive`, `task_aware`, or `mig`. Overrides `memory.compression.pruning_strategy` |
| `--server-compaction` | Enable Claude server-side context compaction (`compact-2026-01-12` beta). Requires a Claude provider. Overrides `llm.cloud.server_compaction` |
| `--extended-context` | Enable Claude 1M extended context window. Tokens above 200K use long-context pricing. Requires a Claude provider. Overrides `llm.cloud.enable_extended_context` |
| `--scan-skills-on-load` | Scan skill content for prompt injection patterns on load. Advisory only — logs warnings; does not block tool calls |
| `--no-pre-execution-verify` | Disable pre-execution verifiers for tool calls. Use in trusted environments when verifiers produce false positives |
| `--policy-file <PATH>` | Path to external policy rules TOML file. Overrides `tools.policy.policy_file` |
| `--dump-format <FORMAT>` | Override debug dump format: `json`, `raw`, or `trace` (OTel OTLP spans) |
| `--scheduler-tick <SECS>` | Override scheduler tick interval in seconds (requires `scheduler` feature) |
| `--scheduler-disable` | Disable the scheduler even if enabled in config (requires `scheduler` feature) |
| `--experiment-run` | Run a single experiment session and exit (requires `experiments` feature). See [Experiments](../concepts/experiments.md) |
| `--experiment-report` | Print past experiment results summary and exit (requires `experiments` feature). See [Experiments](../concepts/experiments.md) |
| `--log-file <PATH>` | Override the log file path for this session. Set to empty string (`""`) to disable file logging. See [Logging](../concepts/logging.md) |
| `--tafc` | Enable Think-Augmented Function Calling for this session, overriding `tools.tafc.enabled`. See [Tools — TAFC](../concepts/tools.md#think-augmented-function-calling-tafc) |
| `--debug-dump [PATH]` | Write LLM requests/responses and raw tool output to files. Omit `PATH` to use `debug.output_dir` from config (default: `.zeph/debug`). See [Debug Dump](../advanced/debug-dump.md) |
| `--plugin-url <URL>` | Load a plugin from a remote URL for this session only (ephemeral). Accepts multiple values. Use `url@sha256` syntax to pin a version, e.g., `--plugin-url https://example.com/plugin.tar.gz@abc123def456`. Requires HTTPS. |
| `--worktree-base-ref <REF>` | Override the base ref for worktree creation: `head` (current HEAD) or `fresh` (clone main). Requires `[worktree] enabled = true`. See [Worktree Isolation](../guides/worktree.md) |
| `--version` | Print version and exit |
| `--help` | Print help and exit |

## Examples

```bash
# Start the agent with defaults
zeph

# Start with a custom config
zeph --config ~/.zeph/config.toml

# Start with TUI dashboard
zeph --tui

# Start with age-encrypted secrets (default paths)
zeph --vault age

# Start with age-encrypted secrets (custom paths)
zeph --vault age --vault-key key.txt --vault-path secrets.age

# Initialize vault and store a secret
zeph vault init
zeph vault set ZEPH_CLAUDE_API_KEY sk-ant-...

# Generate a new config interactively
zeph init

# Start as headless daemon with A2A endpoint
zeph --daemon

# Connect TUI to a running daemon
zeph --connect http://localhost:3000
```
