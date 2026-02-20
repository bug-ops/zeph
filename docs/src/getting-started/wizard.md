# Configuration Wizard

Run `zeph init` to generate a `config.toml` through a guided wizard. This is the fastest way to get a working configuration.

```bash
zeph init
zeph init --output ~/.zeph/config.toml   # custom output path
```

## Step 1: Secrets Backend

Choose how API keys and tokens are stored:

- **env** (default) — read secrets from environment variables
- **age** — encrypt secrets in an age-encrypted vault file (recommended for production)

When `age` is selected, API key prompts in subsequent steps are skipped since secrets are stored via `zeph vault set` instead.

## Step 2: LLM Provider

Select your inference backend:

- **Ollama** — local, free, default. Provide model name (default: `mistral:7b`)
- **Claude** — Anthropic API. Provide API key
- **OpenAI** — OpenAI or compatible API. Provide base URL, model, API key
- **Orchestrator** — multi-model routing. Select a primary and fallback provider
- **Compatible** — any OpenAI-compatible endpoint

Choose an embedding model for skill matching and semantic memory (default: `qwen3-embedding`).

## Step 3: Memory

Set the SQLite database path and optionally enable semantic memory with Qdrant. Qdrant requires a running instance (e.g., via Docker).

## Step 4: Channel

Pick the I/O channel:

- **CLI** (default) — terminal interaction, no setup needed
- **Telegram** — provide bot token, set allowed usernames
- **Discord** — provide bot token and application ID (requires `discord` feature)
- **Slack** — provide bot token and signing secret (requires `slack` feature)

## Step 5: Daemon

Configure headless daemon mode with A2A endpoint (requires `daemon` + `a2a` features):

- **Enable daemon** — toggle daemon supervisor on/off
- **A2A host/port** — bind address for the A2A JSON-RPC server (default: `0.0.0.0:3000`)
- **Auth token** — bearer token for A2A authentication (recommended for production)
- **PID file path** — location for instance detection (default: `~/.zeph/zeph.pid`)

Skip this step if you do not plan to run Zeph in headless mode.

## Step 6: Custom Secrets

If the `age` vault backend was selected, the wizard offers to add custom secrets for skill authentication.

When prompted, enter a secret name and value. The wizard stores each secret with the `ZEPH_SECRET_` prefix in the vault. If any installed skills declare `requires-secrets`, the wizard lists them so you know which keys to provide.

Skip this step if your skills do not require external API credentials.

## Step 7: Update Check

Enable or disable automatic version checks against GitHub Releases (default: enabled).

## Step 8: Review and Save

Inspect the generated TOML, confirm the output path, and save. If the file already exists, the wizard asks before overwriting.

## After the Wizard

The wizard prints the secrets you need to configure:

- **env backend**: `export ZEPH_CLAUDE_API_KEY=...` commands to add to your shell profile
- **age backend**: `zeph vault init` and `zeph vault set` commands to run

## Further Reading

- [Configuration Reference](../reference/configuration.md) — full config file and environment variables
- [Vault — Age Vault](../reference/security.md#age-vault) — vault setup, custom secrets, and Docker integration
