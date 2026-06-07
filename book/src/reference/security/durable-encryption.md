# Durable Journal Encryption

The durable execution layer journals the control flow of an execution — step
results, promise resolutions, and checkpoint snapshots — to a dedicated
`durable.db` database so an interrupted execution can resume rather than restart.
Those payloads can contain sensitive intermediate data, so they are sealed with
an authenticated cipher before they touch disk.

## Cipher

Payloads are encrypted with **XChaCha20-Poly1305** (AEAD), a 192-bit
extended-nonce construction. A fresh random nonce is drawn from the operating
system CSPRNG on every seal, so no nonce-sequencing state has to be persisted and
nonce reuse under a fixed key cannot occur.

The stored blob layout is:

```text
key_id(1 byte) || nonce(24 bytes) || ciphertext || Poly1305 tag(16 bytes)
```

The leading `key_id` byte selects which key decrypts the blob, enabling the
rotation window described below.

### Associated data (tamper-evidence)

Every seal binds the payload to its journal location through the AEAD associated
data: `(execution_id, step_id, entry_kind, idempotency_key)`. As a result a
sealed result cannot be silently relocated — moving a blob to a different step, or
replaying it under a different execution, changes the associated data and makes
decryption fail authentication. A forged or moved entry is rejected (fail-closed)
rather than decrypted into a bogus result.

## Vault key: `ZEPH_DURABLE_KEY`

The cipher key is resolved from the age vault under the key name
`ZEPH_DURABLE_KEY`, never from inline TOML or environment variables (the standard
Zeph vault contract). It is exactly **32 bytes** of high-entropy key material,
**base64-encoded** for storage as a vault string value.

The easiest path is the configuration wizard: `zeph --init` generates a fresh
key and stores it in the age vault automatically when you enable durable
execution. To generate and store it manually instead:

```bash
# Generate 32 random bytes, base64-encode them, and store in the age vault.
head -c 32 /dev/urandom | base64 | zeph vault set ZEPH_DURABLE_KEY --stdin
```

Inspect a journal with decrypted payloads using `zeph durable show <id>
--reveal`, which resolves and decodes this key.

## Encryption requirement (`encrypt_payload`)

AEAD encryption is **on by default** (`[durable].encrypt_payload = true`).
Disabling it is a development-only override and is governed by the deployment:

| Deployment                              | `encrypt_payload = false` |
| --------------------------------------- | ------------------------- |
| Single-user **local SQLite**            | Allowed; logs a startup `WARN` |
| **Shared database** (Postgres / shared) | **Forbidden** — startup error |
| **Restate** backend                     | **Forbidden** — startup error |

The rationale is the trust boundary: a single-user SQLite file inherits the
operating-system file permissions, but a shared or networked database does not,
so the journal must protect its own payloads there.

## Key rotation

The `key_id` byte makes rotation possible without rewriting the journal:

1. Generate a new key and assign it the next `key_id`.
2. Run with the new key as **current** and the old key registered as the
   **previous** key. New entries seal under the new key; in-flight entries sealed
   under the old key still decrypt during this window.
3. Once all executions that used the old key have reached a terminal status
   (drain), remove the old key.

If you prefer not to run a rotation window, the simpler drain-based policy is to
**quiesce** the durable layer — let all running executions reach a terminal
status — before swapping `ZEPH_DURABLE_KEY`. After a clean drain there are no
entries sealed under the old key, so no previous-key window is needed.
