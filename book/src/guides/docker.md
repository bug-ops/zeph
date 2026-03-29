# Docker Deployment

Docker Compose automatically pulls the latest image from GitHub Container Registry. To use a specific version, set `ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.9.8`.

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
docker compose --profile gpu -f docker/docker-compose.yml -f docker/docker-compose.gpu.yml up
```

## PostgreSQL Backend

Zeph supports PostgreSQL as an alternative to the default SQLite backend via the `zeph-db` crate. The `docker-compose.yml` includes a `postgres` service that exposes the `ZEPH_DATABASE_URL` environment variable automatically.

To use PostgreSQL with Docker Compose:

```bash
# Start Zeph with PostgreSQL
ZEPH_DATABASE_URL=postgres://zeph:zeph@localhost:5432/zeph docker compose --profile postgres up
```

Or set `database_url` in your config:

```toml
[memory]
database_url = "postgres://zeph:zeph@localhost:5432/zeph"
```

### Schema Migration

When using PostgreSQL for the first time, or after an upgrade, run the migration CLI to apply schema changes:

```bash
zeph db migrate
```

The `--init` setup wizard includes a backend selection step. Choose **PostgreSQL** to generate a config with `database_url` and the corresponding Docker Compose snippet.

### Environment Variable

`ZEPH_DATABASE_URL` overrides `[memory] database_url` at runtime. This is the recommended way to inject connection strings in containerised deployments rather than embedding credentials in config files:

```bash
ZEPH_DATABASE_URL=postgres://user:pass@db:5432/zeph zeph
```

SQLite remains the default when `database_url` is not set.

## Age Vault (Encrypted Secrets)

```bash
# Mount key and vault files into container
docker compose -f docker/docker-compose.yml -f docker/docker-compose.vault.yml up
```

Override file paths via environment variables:

```bash
ZEPH_VAULT_KEY=./my-key.txt ZEPH_VAULT_PATH=./my-secrets.age \
  docker compose -f docker/docker-compose.yml -f docker/docker-compose.vault.yml up
```

> The image must be built with `vault-age` feature enabled. For local builds, use `CARGO_FEATURES=vault-age` with `docker/docker-compose.dev.yml`.

## Using Specific Version

```bash
# Use a specific release version
ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.9.8 docker compose up

# Always pull latest
docker compose pull && docker compose up
```

## Vulnerability Scanning

Scan the Docker image locally with [Trivy](https://trivy.dev/) before pushing:

```bash
# Scan the latest local image
trivy image ghcr.io/bug-ops/zeph:latest

# Scan a locally built dev image
trivy image zeph:dev

# Fail on HIGH/CRITICAL (useful in CI or pre-push checks)
trivy image --severity HIGH,CRITICAL --exit-code 1 ghcr.io/bug-ops/zeph:latest
```

## Local Development

Full stack with debug tracing (builds from source via `docker/Dockerfile.dev`, uses host Ollama via `host.docker.internal`):

```bash
# Build and start Qdrant + Zeph with debug logging
docker compose -f docker/docker-compose.dev.yml up --build

# Build with optional features (e.g. vault-age, candle)
CARGO_FEATURES=vault-age docker compose -f docker/docker-compose.dev.yml up --build

# Build with vault-age and mount vault files
CARGO_FEATURES=vault-age \
  docker compose -f docker/docker-compose.dev.yml -f docker/docker-compose.vault.yml up --build
```

Dependencies only (run zeph natively on host):

```bash
# Start Qdrant
docker compose -f docker/docker-compose.deps.yml up

# Run zeph natively with debug tracing
RUST_LOG=zeph=debug,zeph_channels=trace cargo run
```
