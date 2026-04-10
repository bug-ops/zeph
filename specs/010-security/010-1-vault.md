---
aliases:
  - Zeph Vault
  - Secret Management
  - Age Encryption
  - Credential Resolution
tags:
  - sdd
  - spec
  - security
  - infra
created: 2026-04-10
status: complete
related:
  - "[[010-security/spec]]"
  - "[[010-2-injection-defense]]"
  - "[[010-3-authorization]]"
  - "[[010-4-audit]]"
---

# Spec: Secret Vault & Credential Resolution

Age-encrypted secret storage, credential resolution, ZEPH_* environment key mapping, vault access control.

## Overview

Zeph stores all secrets (API keys, tokens, passwords) in an encrypted age vault, not in environment variables or `.env` files. The vault is automatically decrypted at startup, and credentials are resolved on-demand by subsystems.

## Key Invariants

**Always:**
- All API keys, tokens, and passwords stored in age vault only
- Vault backend (age/kms/custom) configured once at startup
- Credentials resolved via `vault.get("KEY_NAME")` at runtime, never from env vars
- ZEPH_* environment variables automatically resolved from vault at startup

**Never:**
- Store secrets in `.env`, config files, or command-line arguments
- Pass API keys through shell pipelines or logs
- Hardcode credentials in source code
- Use plaintext file backend in production

## Age Vault Structure

YAML format for quick editing:

```yaml
# ~/.age/zeph.age (encrypted)
---
# OpenAI
openai_api_key: "sk-proj-..."
openai_org_id: "org-..."

# Anthropic
claude_api_key: "sk-ant-..."

# Local APIs
ollama_api_base: "http://localhost:11434"

# Database
postgres_url: "postgresql://user:pass@host/db"

# External Services
telegram_bot_token: "123456:ABCdef..."
github_token: "ghp_..."
```

Encrypted with user's age public key:

```bash
age --encrypt --recipient age1xxx... ~/.age/zeph.age.plaintext > ~/.age/zeph.age
rm ~/.age/zeph.age.plaintext
```

## Vault Access Interface

```rust
pub trait VaultBackend: Send + Sync {
    async fn get(&self, key: &str) -> Result<String>;
    async fn set(&self, key: &str, value: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<String>>;
    async fn delete(&self, key: &str) -> Result<()>;
}

pub struct AgeVault {
    // Decrypted in-memory map after startup
    secrets: Arc<RwLock<HashMap<String, String>>>,
    vault_path: PathBuf,
    identity_path: PathBuf,
}

impl AgeVault {
    async fn load(&mut self) -> Result<()> {
        // 1. Read encrypted file
        let encrypted = fs::read_to_string(&self.vault_path)?;
        
        // 2. Decrypt using age identity
        let decrypted = age_decrypt(&encrypted, &self.identity_path)?;
        
        // 3. Parse YAML
        let parsed: HashMap<String, String> = serde_yaml::from_str(&decrypted)?;
        
        // 4. Store in memory
        *self.secrets.write().await = parsed;
        
        Ok(())
    }
    
    async fn get(&self, key: &str) -> Result<String> {
        self.secrets
            .read()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("Secret '{}' not found in vault", key))
    }
}
```

## Startup Resolution

At agent initialization, populate required ZEPH_* vars:

```rust
async fn resolve_vault_at_startup(vault: &AgeVault) -> Result<()> {
    let required_keys = vec![
        "openai_api_key",
        "claude_api_key",
        "ollama_api_base",
    ];
    
    for key in required_keys {
        if let Ok(value) = vault.get(key).await {
            // Don't set env var; store in config instead
            // ZEPH_OPENAI_API_KEY is resolved lazily from vault
            log::info!("Resolved credential: {}", key);
        } else {
            log::warn!("Missing vault key: {}", key);
        }
    }
    
    Ok(())
}
```

## Lazy Resolution in LLM Providers

Providers request credentials on-demand:

```rust
pub struct ClaudeProvider {
    vault: Arc<AgeVault>,
}

impl ClaudeProvider {
    async fn get_api_key(&self) -> Result<String> {
        self.vault.get("claude_api_key").await
    }
    
    async fn invoke(&self, req: &Request) -> Result<Response> {
        let api_key = self.get_api_key().await?;
        
        // Make request with credential
        let client = reqwest::Client::new();
        client
            .post("https://api.anthropic.com/v1/messages")
            .bearer_auth(&api_key)
            .json(req)
            .send()
            .await?
            .json()
            .await
    }
}
```

## CLI Commands

Management interface:

```bash
# List all vault keys (names only, not values)
cargo run -- vault list

# Get a secret (for external scripts)
cargo run -- vault get openai_api_key

# Set a secret (interactive prompt)
cargo run -- vault set new_key_name

# Validate vault integrity
cargo run -- vault check
```

## Configuration

```toml
[vault]
backend = "age"                    # or "kms", "custom"
vault_path = "~/.age/zeph.age"
identity_path = "~/.age/identity"
fallback_env = false               # don't read from env vars as fallback
```

## Integration Points

- [[003-llm-providers]] — All providers resolve API keys from vault
- [[010-2-injection-defense]] — Vault keys never logged or leaked
- [[010-4-audit]] — Vault access logged for compliance

## See Also

- [[010-security/spec]] — Parent
- [[010-2-injection-defense]] — Prevent key leakage
- [[010-3-authorization]] — Capability-based access to vault
- age encryption: https://age-encryption.org/
