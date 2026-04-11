---
aliases:
  - Vault & Secret Management
  - Age Vault
  - Secret Storage
tags:
  - sdd
  - spec
  - security
  - secrets
  - vault
created: 2026-04-11
status: approved
related:
  - "[[MOC-specs]]"
  - "[[010-security/spec]]"
  - "[[010-security/010-1-vault]]"
  - "[[001-system-invariants/spec#14. Vault Backend Contract]]"
---

# Spec: Vault & Secret Management

> [!info]
> Specification for secret storage in Zeph. Defines the `VaultProvider` trait,
> supported backends (age encryption + env), configuration schema, and security guarantees.

**Scope**: Secret resolution at startup, credential management, API key storage  
**Crate**: `zeph-vault` (Layer 1)  
**Status**: Approved (shipped v0.12.0+)

---

## 1. Overview

Zeph never stores API keys, credentials, or secrets in:
- Config files (`config.toml`)
- Environment variables (except `ZEPH_*` keys resolved from vault)
- Log files or debug output

Instead, all secrets are kept in a **vault** — an age-encrypted JSON file that is decrypted at startup
and kept in memory (in `zeroize::Zeroizing` buffers that erase themselves on drop).

The vault is accessed via the `VaultProvider` trait, which abstracts multiple backend implementations.

---

## 2. VaultProvider Trait

```rust
pub trait VaultProvider: Send + Sync {
    /// Retrieve a secret by key.
    ///
    /// Returns `None` if the key does not exist.
    /// Returns `Err` if decryption or I/O fails.
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>>;
}
```

### 2.1 Key Contract

- **Async**: Secret retrieval is async-safe; callers can `.await` without blocking
- **Fallible**: May return I/O errors, parsing errors, or decryption failures
- **Optional**: Non-existent keys return `Ok(None)`, not an error
- **Zeroized**: All returned values are wrapped in `Secret<String>` (zeroized on drop)

### 2.2 Implementing Custom Backends

To integrate a custom secret store (HashiCorp Vault, AWS Secrets Manager, 1Password):

```rust
struct MyVaultProvider {
    client: MyVaultClient,
}

impl VaultProvider for MyVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>> {
        let client = self.client.clone();
        let key = key.to_owned();
        Box::pin(async move {
            client.fetch(&key)
                .await
                .map_err(|e| VaultError::Backend(e.to_string()))
        })
    }
}
```

---

## 3. Age Vault Backend (Primary)

### 3.1 File Layout

```
~/.config/zeph/
├── vault-key.txt       # age identity (private key), mode 0600
└── secrets.age         # age-encrypted JSON
```

The vault stores secrets as a JSON object encrypted with [age](https://age-encryption.org):

```json
{
  "ZEPH_OPENAI_API_KEY": "sk-proj-...",
  "ZEPH_CLAUDE_API_KEY": "sk-ant-...",
  "ZEPH_GITHUB_TOKEN": "ghp_...",
  "ZEPH_QDRANT_API_KEY": "..."
}
```

### 3.2 Initialization

**First-time setup** (age vault):

1. User runs: `cargo run --features full -- --init`
2. Zeph generates an age keypair: `age-keygen > ~/.config/zeph/vault-key.txt`
3. File is created with Unix permission `0600` (owner RW only)
4. User edits `vault-key.txt` to add secrets manually, or uses `cargo run -- vault set <KEY> <VALUE>`

Alternatively:

```bash
# Generate key
age-keygen | tee ~/.config/zeph/vault-key.txt

# Add secrets (via Zeph CLI)
cargo run --features full -- vault set ZEPH_OPENAI_API_KEY sk-proj-...
```

### 3.3 AgeVaultProvider Implementation

```rust
pub struct AgeVaultProvider {
    secrets: BTreeMap<String, String>,  // decrypted on init
}

impl AgeVaultProvider {
    /// Load and decrypt the age vault from disk.
    ///
    /// # Arguments
    /// - `key_path`: Path to age identity file (e.g., ~/.config/zeph/vault-key.txt)
    /// - `vault_path`: Path to encrypted secrets file (e.g., ~/.config/zeph/secrets.age)
    ///
    /// # Errors
    /// Returns `VaultError` if decryption or parsing fails.
    pub fn new(key_path: &Path, vault_path: &Path) -> Result<Self, VaultError> {
        // 1. Read and parse age identity
        // 2. Read encrypted vault file
        // 3. Decrypt JSON via age
        // 4. Parse JSON → BTreeMap
        // 5. Return AgeVaultProvider with secrets
    }

    /// Persist a secret to the vault (atomic write).
    pub fn set_secret(&mut self, key: &str, value: &str) -> Result<(), VaultError> {
        // 1. Insert key → value into map
        // 2. Serialize to JSON
        // 3. Encrypt with age
        // 4. Write to temp file
        // 5. Rename temp → vault (atomic)
    }
}
```

### 3.4 Security Guarantees

- **Encryption**: Age x25519 with 32-byte keys. Each recipient can only decrypt with their private key
- **At rest**: Secrets are encrypted; only the private key can decrypt them
- **In memory**: All secret values are wrapped in `zeroize::Zeroizing<String>`, which overwrites memory on drop
- **Key file permissions**: Created with `0600` (Unix) to prevent other users from reading the private key
- **Atomic writes**: Updates via temp-file-then-rename prevent corruption on crash
- **No plaintext in logs**: Vault errors do not leak secret values

---

## 4. EnvVaultProvider (Development/Testing Only)

### 4.1 Purpose

For development and CI environments where file-based encryption is overhead,
`EnvVaultProvider` reads secrets from environment variables prefixed with `ZEPH_`.

```bash
export ZEPH_OPENAI_API_KEY="sk-proj-..."
export ZEPH_CLAUDE_API_KEY="sk-ant-..."

cargo run --features full -- --config .local/config/testing.toml
```

### 4.2 Implementation

```rust
pub struct EnvVaultProvider;

impl VaultProvider for EnvVaultProvider {
    fn get_secret(&self, key: &str) -> Pin<Box<dyn Future<...>>> {
        // Simply read from std::env::var(key)
        let value = std::env::var(key).ok();
        Box::pin(async move { Ok(value) })
    }
}
```

### 4.3 Limitations

- **Not for production**: Env vars can be leaked in process listings (`ps aux`)
- **Not secret**: Shell history may retain the value; logs might capture env var values
- **Testing only**: Use only in CI and local development

**CI Usage**:
```bash
# In .github/workflows/test.yml
env:
  ZEPH_VAULT_BACKEND: env
  ZEPH_OPENAI_API_KEY: ${{ secrets.OPENAI_KEY }}
  ZEPH_CLAUDE_API_KEY: ${{ secrets.CLAUDE_KEY }}

- run: cargo test --workspace --features full
```

---

## 5. Configuration: [vault] Section

The `[vault]` section in `config.toml` selects and configures the backend:

```toml
[vault]
backend = "age"                        # or "env"
age_identity = "~/.config/zeph/vault-key.txt"
age_recipients = ["age1..."]           # optional, for multi-recipient encrypted vaults
```

### 5.1 VaultConfig Schema

```rust
pub struct VaultConfig {
    /// Selected backend: "age" | "env"
    pub backend: String,
    
    /// Path to age identity file (only if backend == "age")
    pub age_identity: Option<PathBuf>,
    
    /// Age recipient public keys (only if backend == "age")
    /// If set, vault can be decrypted by multiple identities
    pub age_recipients: Option<Vec<String>>,
}
```

### 5.2 Default Behavior

If `[vault]` section is absent:
- Backend defaults to `"age"`
- `age_identity` defaults to `~/.config/zeph/vault-key.txt`
- `age_recipients` defaults to empty (only this machine's key can decrypt)

---

## 6. Vault Resolution at Startup

In `zeph-core`'s main initialization:

```rust
async fn initialize_vault(config: &Config) -> Result<Arc<dyn VaultProvider>, InitError> {
    match config.vault.backend.as_str() {
        "age" => {
            let identity = config.vault.age_identity
                .as_ref()
                .ok_or_else(|| InitError::Config("age_identity not set".into()))?;
            let vault = AgeVaultProvider::new(identity, VAULT_PATH)?;
            Ok(Arc::new(vault))
        },
        "env" => {
            Ok(Arc::new(EnvVaultProvider))
        },
        unknown => Err(InitError::Config(format!("unknown vault backend: {}", unknown)))
    }
}
```

All `ZEPH_*` environment variables are resolved through this vault at startup:
- `ZEPH_OPENAI_API_KEY` → `vault.get_secret("ZEPH_OPENAI_API_KEY")`
- `ZEPH_CLAUDE_API_KEY` → `vault.get_secret("ZEPH_CLAUDE_API_KEY")`
- etc.

If a key is missing, the startup fails loudly: `Error: secret ZEPH_OPENAI_API_KEY not found in vault`.

---

## 7. Secret Wrapper & Zeroization

The `Secret<T>` type from `zeph-common` wraps all secret values:

```rust
pub struct Secret<T: Zeroize> {
    value: Zeroizing<T>,
}

impl<T: Zeroize> Secret<T> {
    pub fn expose(&self) -> &T { /* ... */ }
    pub fn expose_mut(&mut self) -> &mut T { /* ... */ }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        // Zeroizing<T> overwrites memory with zeros on drop
    }
}
```

**Usage**:

```rust
let api_key = vault.get_secret("ZEPH_OPENAI_API_KEY").await?;
// api_key is Secret<String>, not exposed in logs
println!("{:?}", api_key);  // prints Secret(***)

// To use the value:
let key_str = api_key.expose();
// When api_key is dropped, memory is zeroed
```

---

## 8. Error Handling

The `VaultError` enum covers all failure modes:

```rust
pub enum VaultError {
    /// Age decryption failed (wrong key, corrupted file)
    Decrypt(String),
    
    /// JSON parsing failed
    Parse(String),
    
    /// File I/O error
    Io(IoError),
    
    /// Custom backend error
    Backend(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        // Error messages never leak secret values
        match self {
            VaultError::Decrypt(msg) => write!(f, "vault decryption failed: {}", msg),
            // ...
        }
    }
}
```

---

## 9. CLI Subcommands

The binary exposes CLI commands for vault management:

```bash
# Retrieve a secret (prints to stdout)
cargo run --features full -- vault get ZEPH_OPENAI_API_KEY

# Set a secret (encrypts and writes)
cargo run --features full -- vault set ZEPH_OPENAI_API_KEY sk-proj-...

# List all keys (without values)
cargo run --features full -- vault list

# Generate a new age keypair
cargo run --features full -- vault init-age
```

---

## 10. Key Invariants

### Always
- Secrets are **never** printed in debug output; use `Secret::expose()` with care
- Vault backend is selected at startup and does not change during runtime
- `EnvVaultProvider` is for development/CI only; production uses age
- All secret memory is zeroized on drop — no plaintext leaks
- Vault failures are fatal at startup (missing key → error, not fallback)
- Age vault key file is created with `0600` permissions on Unix

### Ask First
- Using `backend = "env"` in a persistent config (should be `age` or explicit CI handling)
- Sharing the age private key across machines (use different keys per environment)
- Rotating vault secrets (requires re-encrypting entire vault)

### Never
- Store secrets in `config.toml` files
- Commit vault key files (`vault-key.txt`) to git
- Print secret values in logs or error messages
- Fallback to hardcoded defaults if a secret is missing
- Bypass the vault for ANY secret (API keys, tokens, passwords)
- Use `env::var()` directly instead of going through the vault

---

## 11. Multi-Recipient Vaults (Advanced)

For teams where multiple machines or users need to decrypt the same vault:

```toml
[vault]
backend = "age"
age_identity = "~/.config/zeph/vault-key.txt"
age_recipients = [
    "age1alice...",
    "age1bob...",
]
```

The vault is encrypted such that **any** of the listed recipients can decrypt it:

```bash
age-keygen | tee alice-key.txt  # age1alice...
age-keygen | tee bob-key.txt    # age1bob...

# Encrypt for both Alice and Bob
echo '{"SECRET": "value"}' | \
  age -r age1alice... -r age1bob... > vault.age
```

Each user keeps their own private key; both can decrypt the shared vault.

---

## 12. See Also

- [[MOC-specs]] — all specifications
- [[010-security/spec]] — security framework overview
- [[010-security/010-1-vault]] — security spec sub-document on vault
- [[001-system-invariants/spec#14. Vault Backend Contract]] — vault contract
