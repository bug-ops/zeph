# Docker Deployment

Docker Compose automatically pulls the latest image from GitHub Container Registry. To use a specific version, set `ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.9.4`.

## Quick Start (Ollama + Qdrant in containers)

```bash
# Pull Ollama models first
docker compose --profile cpu run --rm ollama ollama pull mistral:7b
docker compose --profile cpu run --rm ollama ollama pull qwen3-embedding

# Start all services
docker compose --profile cpu up
```

## Apple Silicon (Ollama on host with Metal GPU)

```bash
# Use Ollama on macOS host for Metal GPU acceleration
ollama pull mistral:7b
ollama pull qwen3-embedding
ollama serve &

# Start Zeph + Qdrant, connect to host Ollama
ZEPH_LLM_BASE_URL=http://host.docker.internal:11434 docker compose up
```

## Linux with NVIDIA GPU

```bash
# Pull models first
docker compose --profile gpu run --rm ollama ollama pull mistral:7b
docker compose --profile gpu run --rm ollama ollama pull qwen3-embedding

# Start all services with GPU
docker compose --profile gpu -f docker-compose.yml -f docker-compose.gpu.yml up
```

## Age Vault (Encrypted Secrets)

```bash
# Mount key and vault files into container
docker compose -f docker-compose.yml -f docker-compose.vault.yml up
```

Override file paths via environment variables:

```bash
ZEPH_VAULT_KEY=./my-key.txt ZEPH_VAULT_PATH=./my-secrets.age \
  docker compose -f docker-compose.yml -f docker-compose.vault.yml up
```

> The image must be built with `vault-age` feature enabled. For local builds, use `CARGO_FEATURES=vault-age` with `docker-compose.dev.yml`.

## Using Specific Version

```bash
# Use a specific release version
ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.9.4 docker compose up

# Always pull latest
docker compose pull && docker compose up
```

## Local Development

Full stack with debug tracing (builds from source via `Dockerfile.dev`, uses host Ollama via `host.docker.internal`):

```bash
# Build and start Qdrant + Zeph with debug logging
docker compose -f docker-compose.dev.yml up --build

# Build with optional features (e.g. vault-age, candle)
CARGO_FEATURES=vault-age docker compose -f docker-compose.dev.yml up --build

# Build with vault-age and mount vault files
CARGO_FEATURES=vault-age \
  docker compose -f docker-compose.dev.yml -f docker-compose.vault.yml up --build
```

Dependencies only (run zeph natively on host):

```bash
# Start Qdrant
docker compose -f docker-compose.deps.yml up

# Run zeph natively with debug tracing
RUST_LOG=zeph=debug,zeph_channels=trace cargo run
```
