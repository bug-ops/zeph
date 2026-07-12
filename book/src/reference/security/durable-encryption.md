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

"Shared database" is determined by `[durable].shared_db`: set it `true` whenever
the journal database is reachable by more than one process or client (a
network-shared volume, or any future Postgres-backed deployment). It defaults to
`false` for an ordinary single-user local setup. A `postgres://`/`postgresql://`
journal URL is also treated as shared automatically, even if `shared_db` was left
unset, as defense in depth.

## Control-entry HMAC (`EffectIntent` forgery protection)

Some journal entries carry no payload at all — an `EffectIntent` records the
*intent* to run an exactly-once-guarded effect before it fires, so there is
nothing to encrypt. On a shared database these "control" rows still need
tamper-evidence: an attacker who can insert rows directly should not be able to
forge or relocate an `EffectIntent` and trick a resumed execution into skipping
or re-running a guarded effect.

For a declared/detected shared database, every `EffectIntent` row is stamped
with a row-level HMAC over its identity — `(execution_id, step_id, entry_kind,
idempotency_key)` — keyed with a BLAKE3 subkey derived from `ZEPH_DURABLE_KEY`
(domain-separated from the AEAD payload key, so the two keys are
cryptographically independent even though they share one vault secret). Every
read of an `EffectIntent` recomputes and constant-time-verifies this HMAC; a
mismatch — including a row missing its HMAC on a keyed backend — is rejected
fail-closed.

This uses the same `shared_db`/`postgres://`-detection gate described above: a
single-user local, non-shared database never computes or verifies this HMAC
(the row's `hmac` column stays `NULL`), matching the accepted stance that the
DB-file trust boundary already covers that deployment.

This forgery guarantee depends on `shared_db`/`postgres://` being declared
consistently across the writer and every reader of a given journal file; a
reader that disagrees (e.g. runs unkeyed against a keyed writer's file) now
fails closed as soon as it encounters any row carrying a stamped HMAC, rather
than silently trusting it as an ordinary unverified field.

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
