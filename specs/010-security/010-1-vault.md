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

Zeph stores all secrets (API keys, tokens, passwords) in an encrypted age vault, not in environment variables or `.env` files. The vault is automatically decrypted at startup, and credentials are resolved on-demand by subsystems using the pluggable `VaultProvider` trait.

Two backends ship out of the box: `AgeVaultProvider` (recommended, age encryption) and `EnvVaultProvider` (development/testing, reads `ZEPH_SECRET_*` env vars).

## Key Invariants

**Always:**
- All API keys, tokens, and passwords stored in encrypted age vault (production) or env vars (testing only)
- Vault backend configured once at startup via `VaultProvider` trait implementation
- Credentials resolved via `vault.get_secret("KEY_NAME").await` at runtime, never from raw env vars
- Secret values kept in `zeroize::Zeroizing` buffers — automatically zeroed on drop
- Vault file permissions set to `0o600` (owner-read/write only) on Unix

**Never:**
- Store secrets in `.env`, config files, or command-line arguments
- Pass API keys through logging, error messages, or debug output
- Hardcode credentials in source code
- Use synchronous blocking I/O inside async contexts for vault access

## Vault Provider Trait

```rust
pub trait VaultProvider: Send + Sync {
    /// Retrieve a secret by key.
    ///
    /// Returns `Ok(None)` when the key does not exist.
    /// Returns `Err(VaultError)` on backend failures (I/O, decryption, network).
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>>;

    /// Return all known secret keys (optional).
    ///
    /// Default implementation returns an empty `Vec`.
    fn list_keys(&self) -> Vec<String> {
        Vec::new()
    }
}
```

## Age Backend

**File layout:**
```
~/.config/zeph/
├── vault-key.txt   # age identity (private key), mode 0600
└── secrets.age     # age-encrypted JSON: {"KEY": "value", ...}
```

**API:**
```rust
pub struct AgeVaultProvider { /* private */ }

impl AgeVaultProvider {
    /// Load vault from encrypted files.
    pub fn new(key_path: &Path, vault_path: &Path) -> Result<Self, AgeVaultError>;
    
    /// Initialize a new vault with a fresh age keypair.
    pub fn init_vault(dir: &Path) -> Result<(), AgeVaultError>;
    
    /// Synchronous getter for convenience (non-async).
    pub fn get(&self, key: &str) -> Option<&str>;
    
    /// Set or update a secret (requires mutable access).
    pub fn set_secret_mut(&mut self, key: String, value: String, is_new: bool) 
        -> Result<(), AgeVaultError>;
    
    /// Remove a secret.
    pub fn remove_secret_mut(&mut self, key: &str) -> bool;
    
    /// Save encrypted vault to disk (atomic write via `.tmp` suffix).
    pub fn save(&self) -> Result<(), AgeVaultError>;
    
    /// Async variant that offloads I/O to a background thread.
    pub async fn load_async(key_path: &Path, vault_path: &Path) 
        -> Result<Self, AgeVaultError>;
    
    pub async fn save_async(&self) -> Result<(), AgeVaultError>;
}

impl VaultProvider for AgeVaultProvider { /* ... */ }
```

Secrets stored as JSON (not YAML) for forward compatibility:
```json
{
  "ZEPH_CLAUDE_API_KEY": "sk-ant-...",
  "ZEPH_OPENAI_API_KEY": "sk-proj-...",
  "OLLAMA_API_BASE": "http://localhost:11434"
}
```

## Env Backend

Development/testing backend that reads `ZEPH_SECRET_*` prefixed environment variables:

```rust
pub struct EnvVaultProvider;

impl VaultProvider for EnvVaultProvider {
    fn get_secret(&self, key: &str) -> Pin<Box<dyn Future<...>>> {
        // Looks for environment variable with `ZEPH_SECRET_` prefix
        // E.g., key "API_KEY" reads env var "ZEPH_SECRET_API_KEY"
    }
}
```

Used for testing and CI pipelines where file-based vaults are impractical.

## Arc Wrapper

`ArcAgeVaultProvider` wraps `Arc<RwLock<AgeVaultProvider>>` to allow sharing the vault as a trait object while still supporting mutable operations (e.g., OAuth credential persistence):

```rust
pub struct ArcAgeVaultProvider { /* ... */ }

impl VaultProvider for ArcAgeVaultProvider { /* ... */ }

// Mutable methods available via downcasting:
impl ArcAgeVaultProvider {
    pub fn set_secret_mut(&self, key: String, value: String, is_new: bool) 
        -> Result<(), AgeVaultError>;
}
```

## CLI Commands

Management interface:

```bash
# List all vault keys (names only, not values)
cargo run -- vault list

# Get a secret (for external scripts)
cargo run -- vault get ZEPH_OPENAI_API_KEY

# Initialize a new vault with fresh keypair
cargo run -- vault init

# Validate vault integrity
cargo run -- vault check
```

## Configuration

```toml
[vault]
backend = "age"  # "age" (default, recommended), "env" (dev/testing only), or "keyring" (OS keyring)
```

**Vault file path resolution** (for `Age` backend):

Vault files are stored with hardcoded names in a platform-specific config directory, resolved in order:
1. `$XDG_CONFIG_HOME/zeph` (Linux/BSD)
2. `$APPDATA\zeph` (Windows)
3. `$HOME/.config/zeph` (macOS and fallback)

Files within the resolved directory:
- `vault-key.txt` — age private key (created with `0o600` permissions on Unix)
- `secrets.age` — age-encrypted JSON of secrets

**Backend variants:**
- `age` — Recommended for production. Encrypted age vault with private key.
- `env` — Development/testing only. Reads `ZEPH_SECRET_*` environment variables (explicitly set by the user or via `.env` in testing).
- `keyring` — OS-native keyring (macOS Keychain, Windows Credential Manager, Linux Secret Service).

## Integration Points

- [[003-llm-providers/spec]] — All providers resolve API keys from vault
- [[010-2-injection-defense]] — Vault keys never logged or leaked
- [[010-4-audit]] — Vault access logged for compliance

## See Also

- [[010-security/spec]] — Parent
- [[010-2-injection-defense]] — Prevent key leakage
- [[010-3-authorization]] — Capability-based access to vault
- age encryption: https://age-encryption.org/
