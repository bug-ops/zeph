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

When no subcommand is given, Zeph starts the agent loop.

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

### `zeph memory`

Export and import conversation history as portable JSON snapshots.

| Subcommand | Description |
|------------|-------------|
| `memory export <path>` | Export all conversations, messages, and summaries to a JSON file |
| `memory import <path>` | Import a snapshot file into the local database (duplicates are skipped) |

```bash
# Back up all conversation data
zeph memory export backup.json

# Restore on another machine
zeph memory import backup.json
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

## Global Options

| Flag | Description |
|------|-------------|
| `--tui` | Run with the TUI dashboard (requires the `tui` feature) |
| `--daemon` | Run as headless background agent with A2A endpoint (requires `daemon` + `a2a` features). See [Daemon Mode](../guides/daemon-mode.md) |
| `--connect <URL>` | Connect TUI to a remote daemon via A2A SSE streaming (requires `tui` + `a2a` features). See [Daemon Mode](../guides/daemon-mode.md) |
| `--config <PATH>` | Path to a TOML config file (overrides `ZEPH_CONFIG` env var) |
| `--vault <BACKEND>` | Secrets backend: `env` or `age` (overrides `ZEPH_VAULT_BACKEND` env var) |
| `--vault-key <PATH>` | Path to age identity (private key) file (default: `~/.config/zeph/vault-key.txt`, overrides `ZEPH_VAULT_KEY` env var) |
| `--vault-path <PATH>` | Path to age-encrypted secrets file (default: `~/.config/zeph/secrets.age`, overrides `ZEPH_VAULT_PATH` env var) |
| `--graph-memory` | Enable graph-based knowledge memory for this session, overriding `memory.graph.enabled`. See [Graph Memory](../concepts/graph-memory.md) |
| `--lsp-context` | Enable automatic LSP context injection for this session, overriding `agent.lsp.enabled`. Injects diagnostics after file writes and hover info on reads. Requires mcpls MCP server and `lsp-context` feature. See [LSP Code Intelligence](../guides/lsp.md#lsp-context-injection) |
| `--debug-dump [PATH]` | Write LLM requests/responses and raw tool output to files. Omit `PATH` to use `debug.output_dir` from config (default: `.local/debug`). See [Debug Dump](../advanced/debug-dump.md) |
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
