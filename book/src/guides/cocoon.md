# Cocoon Decentralized TEE Provider

[Cocoon](https://cocoon.ai) is a decentralized inference network that executes LLM requests in Trusted Execution Environments (TEEs) on a peer-to-peer network of secure nodes. Zeph supports native integration with optional speech-to-text transcription via the Cocoon sidecar.

Cocoon is particularly useful for:

- **Confidential inference** — Requests execute in hardware-isolated TEEs; no server-side model access
- **Privacy compliance** — End-to-end encrypted communication path with zero-knowledge server operations
- **Flexible deployment** — Run locally with a sidecar or connect to public Cocoon nodes
- **Multi-modal support** — Text chat, tool use, and STT transcription in one provider

## Setup

### Prerequisites

1. Install the Cocoon sidecar (local deployment only):
   ```bash
   # Download from https://cocoon.ai or build from source
   cocoon --version
   ```

2. Start the sidecar on the default port (8765):
   ```bash
   cocoon serve
   # Or on a custom port:
   cocoon serve --port 9000
   ```

### Configuration

Add a Cocoon provider entry to your config:

```toml
[[llm.providers]]
type = "cocoon"
name = "cocoon-local"
base_url = "http://localhost:8765"  # Sidecar endpoint
model = "llama2-7b"                 # Available model on sidecar
```

Or store the base URL in the vault for security:

```bash
zeph vault set ZEPH_COCOON_CLIENT_URL "http://localhost:8765"
```

Then reference it in config:

```toml
[[llm.providers]]
type = "cocoon"
name = "cocoon-local"
base_url = "${ZEPH_COCOON_CLIENT_URL}"
model = "llama2-7b"
```

## Features

### Chat and Streaming

Cocoon supports both single-turn and streaming chat:

```toml
[[llm.providers]]
type = "cocoon"
name = "cocoon"
base_url = "http://localhost:8765"
model = "llama2-7b"
max_tokens = 2048
temperature = 0.7
```

### Tool Use (Function Calling)

Cocoon fully supports tool definitions and structured function calling:

- Define tools in your skills and system prompt
- Zeph automatically formats tool calls for Cocoon
- Streaming tool use is supported with incremental JSON parsing

### Speech-to-Text (STT)

The Cocoon sidecar includes a Whisper-compatible STT endpoint at `/v1/audio/transcriptions`. Configure Zeph to use it:

```toml
[[llm.providers]]
type = "cocoon"
name = "cocoon-stt"
stt_model = "whisper-1"  # Enable STT on this provider
```

When configured, Zeph automatically transcribes voice messages and Telegram audio notes using this provider. See [Audio & Vision](../advanced/multimodal.md) for more details.

### Per-Token Pricing (Cocoon Models)

Unlike cloud providers, Cocoon models may not be in Zeph's built-in pricing table. Configure per-1K-token pricing for accurate cost tracking:

```toml
[[llm.providers]]
type = "cocoon"
name = "cocoon-custom"
base_url = "http://localhost:8765"
model = "my-custom-model"

# Per-1K-token pricing in cents (prompt + completion)
cocoon_pricing = { prompt_cents = 1, completion_cents = 2 }
```

This enables the cost tracker to report accurate token consumption and pricing for your Cocoon inference.

### Multi-Model Routing

Combine Cocoon with other providers for cost-effective multi-tier inference:

```toml
[[llm.providers]]
type = "cocoon"
name = "cocoon-smart"
base_url = "http://localhost:8765"
model = "llama2-13b"

[[llm.providers]]
type = "ollama"
name = "ollama-fast"
base_url = "http://localhost:11434"
model = "qwen3:1.7b"

[llm]
routing = "triage"  # Route by complexity

[llm.complexity_routing]
triage_provider = "ollama-fast"
simple = "ollama-fast"      # Quick questions → fast model
medium = "ollama-fast"      # Moderate tasks → fast model
complex = "cocoon-smart"    # Complex reasoning → TEE
expert = "cocoon-smart"     # Expert tasks → TEE
```

## Diagnostics

Use the `zeph cocoon doctor` command to verify sidecar health and configuration:

```bash
zeph cocoon doctor
```

Output example:

```
Cocoon Diagnostics
==================
Config entry:           [OK] cocoon-local present in config
Sidecar reachability:   [OK] http://localhost:8765/stats
Proxy connection:       [OK] Direct connection established
Worker count:           [OK] 4 workers available
Model listing:          [OK] 7 models available
Vault key resolution:   [OK] ZEPH_COCOON_CLIENT_URL resolved
```

### JSON Output

For automation and scripting, use `--json`:

```bash
zeph cocoon doctor --json
```

## TUI Integration

When using the TUI dashboard with Cocoon enabled, check sidecar status and available models:

- `/cocoon status` — Display sidecar health, worker count, and TON balance
- `/cocoon models` — List all available models on the sidecar

Status updates automatically every 30 seconds in the background.

## Configuration Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | — | Must be `"cocoon"` |
| `name` | string | — | Unique provider identifier |
| `base_url` | string | `"http://localhost:8765"` | Sidecar endpoint URL |
| `model` | string | — | Model name available on the sidecar |
| `stt_model` | string | (optional) | Model to use for speech-to-text |
| `cocoon_pricing` | table | (optional) | Per-1K-token pricing in cents |
| `max_tokens` | integer | 2048 | Max tokens in response |
| `temperature` | float | 0.7 | Sampling temperature |
| `top_p` | float | 1.0 | Nucleus sampling parameter |

## Troubleshooting

### Sidecar Not Reachable

If you see `Cocoon: sidecar unreachable` in the TUI status bar:

1. Verify the sidecar is running:
   ```bash
   curl -s http://localhost:8765/stats | jq .
   ```

2. Check the base URL matches your sidecar port
3. Ensure network connectivity (if sidecar is on a different machine)

### Vault Key Issues

If `zeph cocoon doctor` reports vault key errors:

```bash
# Set the URL in the vault
zeph vault set ZEPH_COCOON_CLIENT_URL "http://localhost:8765"

# Verify it resolves
zeph vault get ZEPH_COCOON_CLIENT_URL
```

### STT Not Working

Verify the Whisper endpoint is available on the sidecar:

```bash
curl -s http://localhost:8765/v1/audio/transcriptions -X OPTIONS
```

If it returns 405 or 404, the sidecar may not have STT support compiled in.

## See Also

- [Audio & Vision](../advanced/multimodal.md) — Configure STT backends and vision models
- [LLM Providers](../concepts/providers.md) — Overview of all supported providers
- [Configuration Reference](../reference/configuration.md) — Full config file documentation
