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
| `vault` | Manage the age-encrypted secrets vault (see [Secrets Management](security.md#age-vault)) |
| `router` | Inspect or reset Thompson Sampling router state (see [Adaptive Inference](../advanced/adaptive-inference.md)) |
| `ingest` | Ingest a document or directory into semantic memory (Qdrant collection) |
| `classifiers` | Manage ML classifier models — list, download, status |
| `sessions` | Manage ACP session history — list, show, delete (requires `acp` feature) |
| `schedule` | Manage cron-based scheduled jobs — list, add, remove, show (requires `scheduler` feature; see [Scheduler](../concepts/scheduler.md)) |
| `db` | Database management — run migrations, check status (see [Database Abstraction](../concepts/database.md)) |
| `migrate-config` | Add missing config parameters as commented-out blocks and reformat the file (see [Migrate Config](../guides/migrate-config.md)) |

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
| `skill trust <name> [level]` | Show or set trust level (`trusted`, `verified`, `quarantined`, `blocked`) |
| `skill block <name>` | Block a skill (deny all tool access) |
| `skill unblock <name>` | Unblock a skill (revert to `quarantined`) |

```bash
# Install from git
zeph skill install https://github.com/user/zeph-skill-example.git

# Install from local path
zeph skill install /path/to/my-skill

# List installed skills
zeph skill list

# Verify integrity and promote trust
zeph skill verify my-skill
zeph skill trust my-skill trusted

# Remove a skill
zeph skill remove my-skill
```

### `zeph plugin`

Manage plugin packages (collections of skills, MCP servers, and config overlays). Installed plugins are stored in `~/.local/share/zeph/plugins/`.

| Subcommand | Description |
|------------|-------------|
| `plugin list` | List installed plugins with installation timestamps |
| `plugin list --overlay` | Show which plugins are active and which were skipped (with reasons), including integrity check failures |
| `plugin add <path>` | Install a plugin from a local directory path (must contain `plugin.toml`) |
| `plugin remove <name>` | Remove an installed plugin by name |

```bash
# List installed plugins
zeph plugin list

# Show the active plugin overlay (useful for diagnosing load failures)
zeph plugin list --overlay

# Install a plugin from a local directory
zeph plugin add /path/to/my-plugin

# Remove a plugin
zeph plugin remove my-plugin
```

**Overlay flag note:** `--overlay` shows which plugins contributed to the active config and which were skipped (with reasons like "integrity mismatch", "invalid manifest", etc.). This is evaluated against the default config — use `--config <path>` in the agent to see the live intersection with your active config.

**Integrity checks:** When you install a plugin, Zeph records a sha256 digest of its `.plugin.toml`. At startup and hot-reload, the digest is verified. If it doesn't match, the plugin is skipped and the mismatch is visible in `plugin list --overlay`. See [Plugin Manifest Integrity](security.md#plugin-manifest-integrity) for details.

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

Manage ACP session history. Requires the `acp` feature.

| Subcommand | Description |
|------------|-------------|
| `sessions list` | List recent ACP sessions with ID, timestamp, and turn count |
| `sessions resume <ID>` | Print all events from a past session to stdout |
| `sessions delete <ID>` | Delete a session and its events from the database |

```bash
zeph sessions list
zeph sessions resume abc123
zeph sessions delete abc123
```

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

## Global Options

| Flag | Description |
|------|-------------|
| `--bare` | Strip the agent to essentials for scripted/CI usage: skips memory initialization, scheduler startup, skill loading, and watcher registration. Faster startup, suitable for piping and non-interactive workflows. Incompatible with `--tui`, `--acp`, and messaging channels |
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
