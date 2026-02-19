# CLI Reference

Zeph uses [clap](https://docs.rs/clap) for argument parsing. Run `zeph --help` for the full synopsis.

## Usage

```
zeph [OPTIONS] [COMMAND]
```

## Subcommands

| Command | Description |
|---------|-------------|
| `init`  | Interactive configuration wizard (see [Configuration](../getting-started/configuration.md)) |
| `vault` | Manage the age-encrypted secrets vault (see [Secrets Management](vault.md)) |

When no subcommand is given, Zeph starts the agent loop with persistent input history (arrow keys to navigate, prefix search with Ctrl+R, line editing via rustyline). History is stored in SQLite and persists across restarts.

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

## Global Options

| Flag | Description |
|------|-------------|
| `--tui` | Run with the TUI dashboard (requires the `tui` feature) |
| `--config <PATH>` | Path to a TOML config file (overrides `ZEPH_CONFIG` env var) |
| `--vault <BACKEND>` | Secrets backend: `env` or `age` (overrides `ZEPH_VAULT_BACKEND` env var) |
| `--vault-key <PATH>` | Path to age identity (private key) file (overrides `ZEPH_VAULT_KEY` env var) |
| `--vault-path <PATH>` | Path to age-encrypted secrets file (overrides `ZEPH_VAULT_PATH` env var) |
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
```
