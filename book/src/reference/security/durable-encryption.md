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

Like the AEAD payload cipher, the control-entry HMAC key has its own rotation
window: verification tries the current key first, falling back to a registered
previous key while a `zeph durable rotate-key` window is open, so a
pre-rotation `EffectIntent` control entry stays readable on every read path
(agent replay, scheduler daemon, and the CLI) through the window. Unlike the
AEAD cipher, the stored `hmac` column carries no key-id selector — control
entries have no payload envelope to carry one — so verification tries both
keys rather than dispatching by an on-disk selector; this is
security-equivalent for the single-slot window `rotate-key` supports.

## High-water-mark (deletion detection)

The AEAD seal and the control-entry HMAC both protect a row's own content and
identity, but neither detects a committed `StepResult` row being deleted
outright. A per-execution high-water-mark closes that gap: a signed
`{key_epoch, max_committed_step_id, committed_result_count}` tuple is updated
in the same transaction as every committed `StepResult`, and verified once on
every resume. Unlike the control-entry HMAC, the high-water-mark is attached
**unconditionally** — including single-user local deployments, which get
deletion detection they would not otherwise have.

The high-water-mark key shares `ZEPH_DURABLE_KEY`'s rotation lifecycle: its
epoch is `[durable] key_id` (current) / `previous_key_id` (previous), the same
fields `rotate-key` drives for the AEAD cipher, so no separate rotation
procedure or flag is needed. A resumed execution whose signed epoch matches
neither the current nor a registered previous key fails closed as
"possibly re-keyed" rather than a generic tamper report, distinguishing a
legitimate rotation the process cannot resolve from actual tampering; either
way the resume is refused with no interactive override.

## Key rotation

The `key_id` byte makes rotation possible without rewriting the journal.
`zeph durable rotate-key` drives the whole procedure:

```bash
# Open a window: generates a fresh ZEPH_DURABLE_KEY, stashes the old key under
# ZEPH_DURABLE_KEY_PREVIOUS, and bumps [durable] key_id / previous_key_id in the config.
zeph durable rotate-key

# Preview what would change without writing anything.
zeph durable rotate-key --dry-run
```

New payloads seal under the new key; payloads sealed before the rotation still
decrypt through the registered previous key. Only **one** rotation window is
open at a time — running `rotate-key` again while a window is already open is
refused (the cipher has a single previous-key slot; a second rotation would
silently orphan the first previous key), so close the current window first.

The cipher is built once at process startup and does not hot-reload, so **a
restart is required** after rotating for every consumer (agent process,
scheduler daemon, `--reveal`, the TUI durable panel) to pick up the new key.

Once every execution that used the old key has reached a terminal status and
been pruned — the default retention window is roughly 30 days; see
`[durable.retention]` — close the window:

```bash
zeph durable rotate-key --drop-previous
```

This removes `ZEPH_DURABLE_KEY_PREVIOUS` from the vault and clears
`previous_key_id`. By default it runs three independent safety scans and
refuses the drop if any finds a surviving dependency on the previous key:

- an AEAD blob-scan, refusing if any payload is still sealed under the old key,
- a control-entry HMAC scan, refusing if any `EffectIntent` still verifies only
  under the previous key (catching a payload-less crash-orphaned intent the
  blob-scan cannot see), and
- a high-water-mark scan, refusing if any execution's signed high-water-mark
  still carries the previous key epoch (catching a checkpoint-folded
  pre-rotation execution, whose payload and control entries may both already
  be gone even though its high-water-mark has not migrated).

Pass `--force` to skip all three scans once you have independently confirmed
pruning is complete. Payloads and control/high-water-mark state still bound to
the dropped key become permanently unreadable afterward. A call with no window
open is a clean no-op.

On a **shared database** (`[durable].shared_db = true`, or a `postgres://`
journal URL), rotating also changes the derived control-entry HMAC key — this
now has its own rotation window (see above), so shared-database rotation needs
no special acknowledgement and works exactly like a single-user local
rotation.

`zeph --init`'s wizard step can also replace `ZEPH_DURABLE_KEY`, but that path
is a **destructive reset**: it discards the old key immediately with no
rotation window, orphaning every existing sealed payload right away. Prefer
`zeph durable rotate-key` unless you specifically want to discard every
existing payload and start over.
