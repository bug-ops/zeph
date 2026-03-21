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

## Step 5: Update Check

Enable or disable automatic version checks against GitHub Releases (default: enabled).

## Step 6: Scheduler

Configure the cron-based task scheduler (requires `scheduler` feature):

- **Enable scheduler** — toggle scheduled task execution on/off
- **Tick interval** — how often the scheduler polls for due tasks in seconds (default: 60)
- **Max tasks** — maximum number of scheduled tasks (default: 100)

Skip this step if you do not use scheduled tasks.

## Step 7: Orchestration

Configure multi-agent task orchestration (requires `orchestration` feature):

- **Enable orchestration** — toggle task graph execution on/off
- **Max tasks per graph** — upper bound on tasks per `/plan` invocation (default: 20)
- **Max parallel tasks** — concurrency limit for task execution (default: 4)
- **Require confirmation** — show plan summary and ask `/plan confirm` before executing (default: true)
- **Failure strategy** — how to handle task failures: `abort`, `retry`, `skip`, or `ask`
- **Planner model** — LLM override for plan generation (empty = agent's primary model)

## Step 8: Daemon

Configure headless daemon mode with A2A endpoint (requires `daemon` + `a2a` features):

- **Enable daemon** — toggle daemon supervisor on/off
- **A2A host/port** — bind address for the A2A JSON-RPC server (default: `0.0.0.0:3000`)
- **Auth token** — bearer token for A2A authentication (recommended for production)
- **PID file path** — location for instance detection (default: `~/.zeph/zeph.pid`)

Skip this step if you do not plan to run Zeph in headless mode.

## Step 9: ACP

Configure the Agent Client Protocol server (requires `acp` feature):

- **Agent name** — name advertised in the ACP manifest (default: `zeph`)
- **Agent version** — version string for the manifest (defaults to the binary version)

## Step 10: LSP Code Intelligence

Configure LSP code intelligence via mcpls:

- **Enable LSP via mcpls** — expose 16 LSP tools (hover, definition, references, diagnostics, call hierarchy, rename, and more) to the agent through the MCP client
- **Workspace root(s)** — one or more project directories for mcpls to index; defaults to the current directory

When enabled, the wizard generates an `[[mcp.servers]]` block with `command = "mcpls"` and a 60-second timeout (LSP servers need warmup time). If `mcpls` is not found in PATH, the wizard prints the install command: `cargo install mcpls`.

After answering this step, the wizard prompts for **LSP context injection** (requires the `lsp-context`
feature):

- **Enable automatic LSP context injection** — automatically inject diagnostics after `write_file`
  calls so the agent sees compiler errors without making explicit tool calls. Defaults to enabled when
  mcpls is configured. Skipped automatically when mcpls is not enabled.

When enabled, the wizard generates an `[agent.lsp]` config section with `enabled = true` and
default sub-section values.

See [LSP Code Intelligence](../guides/lsp.md) for full setup details, including hover-on-read and
references-on-rename configuration.

## Step 11: Sub-Agents


Configure the sub-agent system:

- **Enable sub-agents** — toggle parallel sub-agent execution
- **Max concurrent** — maximum sub-agents running at the same time (default: 1)

## Step 12: Router

Configure the Thompson Sampling model router (requires `router` feature):

- **Enable router** — toggle router on/off
- **State file path** — where to persist alpha/beta statistics (default: `~/.zeph/router_thompson_state.json`)

## Step 13: Experiments

Configure autonomous self-experimentation:

- **Enable autonomous experiments** — toggle the experiment engine on/off (default: disabled)
- **Judge model** — model used for LLM-as-judge evaluation (default: `claude-sonnet-4-20250514`)
- **Schedule automatic runs** — enable cron-based experiment sessions (default: disabled)
- **Cron schedule** — 5-field cron expression for scheduled runs (default: `0 3 * * *`, daily at 03:00)

When enabled, the agent can autonomously tune its own inference parameters by running A/B trials against a benchmark dataset. See [Experiments](../concepts/experiments.md) for details.

## Step 14: Self-Learning

Configure the self-learning feedback detector:

- **Correction detection strategy** — `regex` (default) or `judge`
  - **regex** — pattern matching only, zero extra LLM calls
  - **judge** — LLM-backed classifier for borderline cases; you can specify a dedicated model
- **Correction confidence threshold** — Jaccard overlap threshold (default: 0.7)

## Step 15: Compaction Probe

Configure post-compression context integrity validation:

- **Enable compaction probe** — validate summary quality after each hard compaction event (default: disabled)
- **Probe model** — model for probe LLM calls; leave empty to use the summary provider (default: empty)
- **Pass threshold** — minimum score for the Pass verdict (default: 0.6)
- **Hard fail threshold** — score below this blocks compaction entirely (default: 0.35)
- **Max questions** — number of factual questions generated per probe (default: 3)

When enabled, each hard compaction is followed by a quality check. If the summary fails to preserve critical facts (HardFail), compaction is blocked and original messages are preserved. See [Context Engineering — Compaction Probe](../advanced/context.md#post-compression-validation-compaction-probe) for tuning guidance.

## Step 16: Debug Dump

Enable debug dump at startup:

- **Enable debug dump** — write LLM requests/responses and raw tool output to numbered files in `.zeph/debug` (default: disabled)

Debug dump is intended for context debugging — use it when you need to inspect exactly what is sent to the LLM and what comes back. See [Debug Dump](../advanced/debug-dump.md) for details.

## Step 17: Security

Configure security features:

- **PII filter** — scrub emails, phone numbers, SSNs, and credit card numbers from tool outputs before they reach the LLM context and debug dumps (default: disabled)
- **Tool rate limiter** — sliding-window per-category limits (shell 30/min, web 20/min, memory 60/min) to prevent runaway tool calls (default: disabled)
- **Skill scan on load** — scan skill content for injection patterns when skills are loaded; logs warnings but does not block execution (default: enabled)
- **Pre-execution verification** — block destructive commands (e.g. `rm -rf /`) and injection patterns before every tool call (default: enabled)
  - **Allowed paths** — comma-separated path prefixes where destructive commands are permitted (empty = deny all). Example: `/tmp,/home/user/scratch`
  - Shell tools checked by default: `bash`, `shell`, `terminal` (configurable in `config.toml` via `security.pre_execution_verify.destructive_commands.shell_tools`)
- **Guardrail** (requires `guardrail` feature) — LLM-based prompt injection pre-screening via a dedicated safety model (e.g. `llama-guard-3:1b`)

## Step 18: Review and Save

Inspect the generated TOML, confirm the output path, and save. If the file already exists, the wizard asks before overwriting.

## After the Wizard

The wizard prints the secrets you need to configure:

- **env backend**: `export ZEPH_CLAUDE_API_KEY=...` commands to add to your shell profile
- **age backend**: `zeph vault init` and `zeph vault set` commands to run

## Further Reading

- [Configuration Reference](../reference/configuration.md) — full config file and environment variables
- [Vault — Age Vault](../reference/security.md#age-vault) — vault setup, custom secrets, and Docker integration
