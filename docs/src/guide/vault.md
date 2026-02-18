# Secrets Management

Zeph resolves secrets (`ZEPH_CLAUDE_API_KEY`, `ZEPH_OPENAI_API_KEY`, `ZEPH_TELEGRAM_TOKEN`, `ZEPH_A2A_AUTH_TOKEN`) through a pluggable `VaultProvider` with redacted debug output via the `Secret` newtype.

> Never commit secrets to version control. Use environment variables or age-encrypted vault files.

## Backend Selection

The vault backend is determined by the following priority (highest to lowest):

1. **CLI flag:** `--vault env` or `--vault age`
2. **Environment variable:** `ZEPH_VAULT_BACKEND`
3. **Config file:** `vault.backend` in TOML config
4. **Default:** `"env"`

## Backends

| Backend | Description | Activation |
|---------|-------------|------------|
| `env` (default) | Read secrets from environment variables | `--vault env` or omit |
| `age` | Decrypt age-encrypted JSON vault file at startup | `--vault age --vault-key <identity> --vault-path <vault.age>` |

## Environment Variables (default)

Export secrets as environment variables:

```bash
export ZEPH_CLAUDE_API_KEY=sk-ant-...
export ZEPH_TELEGRAM_TOKEN=123:ABC
./target/release/zeph
```

## Age Vault

For production deployments, encrypt secrets with [age](https://age-encryption.org/):

```bash
# Generate an age identity key
age-keygen -o key.txt

# Create a JSON secrets file and encrypt it
echo '{"ZEPH_CLAUDE_API_KEY":"sk-...","ZEPH_TELEGRAM_TOKEN":"123:ABC"}' | \
  age -r $(grep 'public key' key.txt | awk '{print $NF}') -o secrets.age

# Run with age vault
cargo build --release --features vault-age
./target/release/zeph --vault age --vault-key key.txt --vault-path secrets.age
```

> The `vault-age` feature flag is enabled by default. When building with `--no-default-features`, add `vault-age` explicitly if needed.

## Docker

Mount key and vault files into the container:

```bash
docker compose -f docker/docker-compose.yml -f docker/docker-compose.vault.yml up
```

Override paths:

```bash
ZEPH_VAULT_KEY=./my-key.txt ZEPH_VAULT_PATH=./my-secrets.age \
  docker compose -f docker/docker-compose.yml -f docker/docker-compose.vault.yml up
```
