// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for the `zeph durable` CLI command group.
//!
//! Connects directly to the dedicated `durable.db` journal — no running agent process is required
//! (FR-DE-08). Default output is redacted: payload bytes and resolver tokens are shown only with
//! `--reveal`, which decrypts through the vault-resolved `ZEPH_DURABLE_KEY` (INV-5, FR-DE-07).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;

use crate::bootstrap::load_config_or_default;
use zeph_core::config::{Config, DurableConfig};
use zeph_core::durable::XChaCha20Poly1305Cipher;
use zeph_core::vault::AgeVaultProvider;
use zeph_durable::{CancelOutcome, ExecutionId, Journal, LocalBackend};

use crate::cli::DurableCommand;

/// Printed before any decrypted payload is shown, so the operator knows plaintext is on screen.
const REVEAL_WARNING: &str =
    "WARNING: --reveal decrypts and prints payload bytes in cleartext. Do not share this output.";

/// Vault secret name for the current AEAD payload key.
const CURRENT_KEY_VAULT_NAME: &str = "ZEPH_DURABLE_KEY";
/// Vault secret name for the previous AEAD payload key, present only while a `zeph durable
/// rotate-key` rotation window is open (`config.durable.previous_key_id.is_some()`).
const PREVIOUS_KEY_VAULT_NAME: &str = "ZEPH_DURABLE_KEY_PREVIOUS";

/// Resolve the dedicated durable journal path: a sibling of the main database file.
///
/// The durable journal lives in its own file on its own pool (INV-14), next to `memory.sqlite_path`.
/// Shared with the TUI durable poll task so both target the same file.
///
/// The journal file name is namespaced by the main DB's full file name (e.g. `zeph.db.durable.db`
/// for `zeph.db`) rather than a bare `durable.db`, so two distinct memory databases living in the
/// same directory never share a journal file — sharing one previously made a fresh conversation
/// in one DB collide with an unrelated `ExecutionId` from the other DB's journal, since both
/// databases' first-ever conversation gets the same small-integer `ConversationId` (#5553). The
/// full file name (not just the stem) is used so that same-stem, different-extension DBs (e.g.
/// `zeph.db` and `zeph.sqlite`) also resolve to distinct journal files.
///
/// A pre-existing bare `durable.db` in the directory is still preferred when present, so an
/// upgrade does not orphan that *file*. Note this does not make old in-flight P1 agent-turn
/// executions inside it resumable — the `ExecutionId` derivation also changed (folds in
/// `sqlite_path`, see `durable_bootstrap.rs`), so a pre-upgrade execution simply is not found and
/// a fresh one starts; this is a one-time, low-impact effect at the upgrade boundary only.
pub(crate) fn resolve_durable_db_url(config: &Config) -> String {
    let main = config.memory.sqlite_path.as_str();
    let Some(dir) = Path::new(main)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    else {
        return "durable.db".to_owned();
    };

    let legacy = dir.join("durable.db");
    if legacy.exists() {
        return legacy.to_string_lossy().into_owned();
    }

    let file_name = Path::new(main).file_name().map_or_else(
        || "zeph.db".to_owned(),
        |s| s.to_string_lossy().into_owned(),
    );
    dir.join(format!("{file_name}.durable.db"))
        .to_string_lossy()
        .into_owned()
}

/// Format a Unix-epoch-millisecond timestamp as a readable UTC string, falling back to the raw value.
fn fmt_ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

/// Whether the durable journal at `url` must be treated as a shared, multi-client database for
/// the INV-8 AEAD gate (`zeph_durable::encryption_gate`).
///
/// Trusts the operator-declared `config.durable.shared_db` flag, but also recognizes a
/// `postgres://`/`postgresql://` URL scheme as shared-by-construction — defense in depth so a
/// future Postgres-backed `resolve_durable_db_url` is caught by the gate even before the flag is
/// set for that deployment.
fn is_shared_db(config: &zeph_core::config::DurableConfig, url: &str) -> bool {
    config.shared_db || url.starts_with("postgres://") || url.starts_with("postgresql://")
}

/// Evaluate the INV-8 AEAD enforcement policy for the resolved durable journal `url` and emit the
/// documented startup warning for the permitted `encrypt_payload = false` override.
///
/// Shared by every durable-journal entry point that opens a `LocalBackend` — the CLI write path
/// ([`load_write_cipher`]), the CLI read path ([`open_backend`]), and the TUI durable panel poller
/// (`durable_poll_task` in `tui_bridge.rs`) — so a misconfigured shared-DB deployment is rejected
/// consistently regardless of which surface touches the journal first (#5996).
///
/// # Errors
///
/// Returns an error when [`zeph_durable::encryption_gate`] rejects the configuration (a
/// non-local backend or a declared/detected shared database combined with `encrypt_payload =
/// false`).
pub(crate) fn enforce_encryption_gate(
    config: &zeph_core::config::DurableConfig,
    url: &str,
) -> anyhow::Result<()> {
    let shared_db = is_shared_db(config, url);
    match zeph_durable::encryption_gate(config, shared_db) {
        Ok(zeph_durable::EncryptionGate::Enabled) => Ok(()),
        Ok(zeph_durable::EncryptionGate::DisabledLocalWarn) => {
            tracing::warn!(
                "durable: AEAD payload encryption is disabled (encrypt_payload = false); \
                 journal payloads are stored in plaintext. This is a development-only override, \
                 permitted only for a single-user local, non-shared database (INV-8)."
            );
            Ok(())
        }
        Err(e) => Err(anyhow::Error::new(e).context("durable execution security policy")),
    }
}

/// Build the AEAD cipher from the vault-stored `ZEPH_DURABLE_KEY`, honoring an open
/// `zeph durable rotate-key` rotation window (`config.previous_key_id`) when one is declared.
///
/// The single chokepoint every consumer routes through: the CLI write path
/// ([`load_write_cipher`]), the CLI read path ([`open_backend`]/`--reveal`), and the TUI durable
/// poll task all resolve their cipher here, so a rotation window opened via `zeph durable
/// rotate-key` is picked up uniformly (#6447).
///
/// The `previous_key_id` lookup is a **hard error**, never a best-effort skip, when
/// `ZEPH_DURABLE_KEY_PREVIOUS` is declared but missing from the vault: this is the fail-closed
/// branch a config-first rotation write depends on for crash safety — a crash between the config
/// write and the vault write leaves exactly this state, and failing loud here (rather than
/// silently building a cipher with no previous slot) is what prevents every pre-rotation payload
/// from silently becoming undecryptable.
///
/// # Errors
///
/// Returns an error if the vault cannot be loaded, `ZEPH_DURABLE_KEY` is absent or malformed, or
/// `previous_key_id` is set but `ZEPH_DURABLE_KEY_PREVIOUS` is missing or malformed.
fn load_durable_cipher(config: &DurableConfig) -> anyhow::Result<XChaCha20Poly1305Cipher> {
    let dir = zeph_core::vault::default_vault_dir();
    let provider = AgeVaultProvider::load(&dir.join("vault-key.txt"), &dir.join("secrets.age"))
        .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
    let key = provider.get(CURRENT_KEY_VAULT_NAME).ok_or_else(|| {
        anyhow::anyhow!("{CURRENT_KEY_VAULT_NAME} not found in vault; cannot --reveal payloads")
    })?;
    let mut cipher = XChaCha20Poly1305Cipher::from_vault_b64_with_id(config.key_id, key)
        .map_err(|e| anyhow::anyhow!("invalid {CURRENT_KEY_VAULT_NAME}: {e}"))?;

    if let Some(prev_id) = config.previous_key_id {
        let prev_key = provider.get(PREVIOUS_KEY_VAULT_NAME).ok_or_else(|| {
            anyhow::anyhow!(
                "durable config declares previous_key_id = {prev_id} but \
                 {PREVIOUS_KEY_VAULT_NAME} is missing from the vault; refusing to build a \
                 cipher with an inconsistent rotation state (this can happen if a previous \
                 `zeph durable rotate-key` run crashed mid-write) — reconcile config and vault \
                 to a consistent pair, or re-run `zeph durable rotate-key` to see recovery \
                 guidance, before retrying"
            )
        })?;
        let prev_bytes = zeph_core::durable::decode_vault_key_bytes(prev_key)
            .map_err(|e| anyhow::anyhow!("invalid {PREVIOUS_KEY_VAULT_NAME}: {e}"))?;
        cipher = cipher.with_previous(prev_id, prev_bytes);
    }

    Ok(cipher)
}

/// Resolve the control-entry row HMAC key (INV-8) for the durable journal at `url`, deriving it
/// from the vault-stored `ZEPH_DURABLE_KEY` when this deployment is a declared/detected shared
/// database ([`is_shared_db`]).
///
/// Returns `Ok(None)` for a single-user local, non-shared database — the documented stance where
/// control entries carry no HMAC and none is enforced (INV-8). Fails closed (`Err`) when the
/// deployment is shared but `ZEPH_DURABLE_KEY` cannot be resolved or decoded: a shared database
/// must never silently run without the row-HMAC forgery defense.
fn load_control_hmac_key(
    config: &zeph_core::config::DurableConfig,
    url: &str,
) -> anyhow::Result<Option<[u8; 32]>> {
    if !is_shared_db(config, url) {
        return Ok(None);
    }
    let dir = zeph_core::vault::default_vault_dir();
    let provider = AgeVaultProvider::load(&dir.join("vault-key.txt"), &dir.join("secrets.age"))
        .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
    let key = provider.get("ZEPH_DURABLE_KEY").ok_or_else(|| {
        anyhow::anyhow!(
            "ZEPH_DURABLE_KEY not found in vault; required to compute the control-entry row HMAC \
             on a shared database (INV-8)"
        )
    })?;
    let hmac_key = zeph_core::durable::derive_control_hmac_key_b64(key).map_err(|e| {
        anyhow::anyhow!("invalid ZEPH_DURABLE_KEY for control-entry HMAC derivation: {e}")
    })?;
    Ok(Some(hmac_key))
}

/// Resolve the control-entry HMAC key to attach on a durable *write* path (INV-8), mirroring
/// [`load_write_cipher`]'s `config` → `url` resolution.
///
/// # Errors
///
/// Returns an error when this deployment is a shared database and `ZEPH_DURABLE_KEY` cannot be
/// resolved from the vault.
pub(crate) fn load_write_hmac_key(config: &Config) -> anyhow::Result<Option<[u8; 32]>> {
    let url = resolve_durable_db_url(config);
    load_control_hmac_key(&config.durable, &url)
}

/// Resolve the AEAD payload cipher to attach on a durable *write* path when
/// `config.durable.encrypt_payload` is enabled (INV-5).
///
/// First evaluates the INV-8 `encryption_gate` security policy: `encrypt_payload = false` is
/// rejected outright on a non-local backend or a declared/detected shared database (#5996), and
/// emits a startup `WARN` for the permitted single-user local override.
///
/// Returns `Ok(None)` when encryption is disabled — the documented dev-only override where
/// payloads are stored as plaintext, so no cipher is attached and writes stay unchanged.
/// Returns `Ok(Some(cipher))` when encryption is enabled and `ZEPH_DURABLE_KEY` resolves from
/// the vault. Fails closed (`Err`) when encryption is enabled but the key is missing: a write
/// path must never silently persist plaintext while the config declares payloads encrypted.
pub(crate) fn load_write_cipher(
    config: &Config,
) -> anyhow::Result<Option<Arc<dyn zeph_durable::PayloadCipher>>> {
    let url = resolve_durable_db_url(config);
    enforce_encryption_gate(&config.durable, &url)?;

    if !config.durable.encrypt_payload {
        return Ok(None);
    }
    let cipher = load_durable_cipher(&config.durable)?;
    Ok(Some(Arc::new(cipher)))
}

/// Open the local durable backend, attaching the AEAD cipher when `reveal` is set and payloads are
/// actually encrypted (`config.durable.encrypt_payload`). When `encrypt_payload` is `false` (the
/// documented dev-only override), stored payloads are already plaintext, so `--reveal` must not
/// require `ZEPH_DURABLE_KEY` to be present in the vault.
///
/// First evaluates the INV-8 `encryption_gate` security policy (see [`enforce_encryption_gate`]):
/// a declared/detected shared database with `encrypt_payload = false` is rejected on this read
/// path too — reading a plaintext journal on a shared database is still the forbidden state the
/// gate exists to reject, regardless of whether the read is via a write path or `--reveal`
/// (#5996). The permitted single-user local override still emits a startup `WARN` and proceeds.
///
/// Also attaches the control-entry HMAC key ([`load_control_hmac_key`]) whenever this deployment
/// is a declared/detected shared database, unconditionally of `reveal` — HMAC verification guards
/// every read of a control entry (`list`/`show`/`inspect`/`prune`/`resume`/`--reveal` alike), not
/// just the decrypted view (#6043/#6044).
///
/// Returns `Ok(None)` when no journal file exists yet (a friendly signal that durable execution has
/// not run on this deployment), so the caller can print guidance instead of creating an empty file.
async fn open_backend(config: &Config, reveal: bool) -> anyhow::Result<Option<LocalBackend>> {
    let url = resolve_durable_db_url(config);
    enforce_encryption_gate(&config.durable, &url)?;
    let hmac_key = load_control_hmac_key(&config.durable, &url)?;
    if url != ":memory:" && !Path::new(&url).exists() {
        println!(
            "No durable journal at {url}.\n\
             Durable execution may be disabled; enable it with `[durable] enabled = true`."
        );
        return Ok(None);
    }
    let backend = LocalBackend::open(&url, config.durable.max_payload_bytes)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open durable journal: {e}"))?;
    backend
        .init()
        .await
        .map_err(|e| anyhow::anyhow!("failed to initialize durable schema: {e}"))?;
    let backend = if let Some(key) = hmac_key {
        backend.with_hmac_key(key)
    } else {
        backend
    };
    if reveal && config.durable.encrypt_payload {
        let cipher = load_durable_cipher(&config.durable)?;
        Ok(Some(backend.with_cipher(Arc::new(cipher))))
    } else {
        Ok(Some(backend))
    }
}

/// Dispatch a `zeph durable` subcommand.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded, the journal cannot be opened, or a query fails.
#[allow(clippy::too_many_lines)]
pub(crate) async fn handle_durable_command(
    cmd: DurableCommand,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let config_file = crate::bootstrap::resolve_config_path(config_path);
    let config = load_config_or_default(&config_file);

    match cmd {
        DurableCommand::List {
            status,
            kind,
            limit,
            json,
        } => {
            let Some(backend) = open_backend(&config, false).await? else {
                return Ok(());
            };
            let rows = backend
                .list_executions(status.as_deref(), kind.as_deref(), limit)
                .await
                .map_err(|e| anyhow::anyhow!("failed to list executions: {e}"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows)
                        .context("failed to serialize execution list")?
                );
            } else {
                if rows.is_empty() {
                    println!("No durable executions match.");
                    return Ok(());
                }
                println!(
                    "{:<36} {:<18} {:<10} {:>6}  CREATED",
                    "EXECUTION ID", "KIND", "STATUS", "STEPS"
                );
                println!("{}", "-".repeat(96));
                for row in &rows {
                    println!(
                        "{:<36} {:<18} {:<10} {:>6}  {}",
                        row.execution_id.as_uuid(),
                        row.kind,
                        row.status.as_str(),
                        row.step_count,
                        fmt_ts(row.created_at_ms),
                    );
                }
            }
        }

        DurableCommand::Show { id, reveal, json } => {
            let exec = ExecutionId::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid execution id '{id}': {e}"))?;
            let Some(backend) = open_backend(&config, reveal).await? else {
                return Ok(());
            };
            show_entries(&backend, exec, reveal, None, json).await?;
        }

        DurableCommand::Inspect {
            id,
            step,
            reveal,
            json,
        } => {
            let exec = ExecutionId::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid execution id '{id}': {e}"))?;
            let Some(backend) = open_backend(&config, reveal).await? else {
                return Ok(());
            };
            show_entries(&backend, exec, reveal, Some(step), json).await?;
        }

        DurableCommand::Prune { dry_run } => {
            let Some(backend) = open_backend(&config, false).await? else {
                return Ok(());
            };
            let policy = &config.durable.retention;
            if dry_run {
                let orphans = backend
                    .count_orphans(policy)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to count orphaned executions: {e}"))?;
                println!("Dry run: {orphans} orphaned execution(s) would be aborted.");
                let n = backend
                    .count_prunable(policy)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to count prunable executions: {e}"))?;
                println!("Dry run: {n} terminal execution(s) past TTL would be pruned.");
            } else {
                let aborted = backend
                    .sweep_orphans(policy)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to sweep orphaned executions: {e}"))?;
                println!("Aborted {aborted} orphaned execution(s).");
                let n = backend
                    .prune(policy)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to prune journal: {e}"))?;
                println!("Pruned {n} execution(s).");
            }
        }

        DurableCommand::Resume { id } => {
            let exec = ExecutionId::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid execution id '{id}': {e}"))?;
            let Some(backend) = open_backend(&config, false).await? else {
                return Ok(());
            };
            // FR-011: a canceled execution is refused distinctly, before the generic
            // adapters-not-wired message — resuming it would contradict the operator's own
            // deliberate `zeph durable cancel` decision (INV-16′).
            if backend
                .execution_status(exec)
                .await
                .map_err(|e| anyhow::anyhow!("failed to look up execution status: {e}"))?
                == Some(zeph_durable::ExecutionStatus::Canceled)
            {
                println!("Execution {id} was intentionally canceled and will not be resumed.");
                return Ok(());
            }
            let entries = backend
                .read_execution_redacted(exec)
                .await
                .map_err(|e| anyhow::anyhow!("failed to read execution: {e}"))?;
            if entries.is_empty() {
                println!("No journal entries found for execution {id}.");
                return Ok(());
            }
            // Resume semantics are owned by the per-kind adapters (epic #4707, A1-A4); none are wired
            // in this build. Report state honestly rather than pretending to replay.
            println!(
                "Execution {id} has {} journaled step(s).\n\
                 Automatic resume is performed by the agent process for supported execution kinds; \
                 standalone CLI replay is not available in this build (durable adapters A1-A4).",
                entries.len()
            );
        }

        DurableCommand::Cancel { id } => {
            let exec = ExecutionId::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid execution id '{id}': {e}"))?;
            let Some(backend) = open_backend(&config, false).await? else {
                return Ok(());
            };
            let outcome = backend
                .cancel_execution(exec)
                .await
                .map_err(|e| anyhow::anyhow!("failed to cancel execution: {e}"))?;
            match outcome {
                CancelOutcome::Canceled => {
                    println!("Execution {id} canceled; it will not be resumed.");
                }
                CancelOutcome::AlreadyTerminal { status } => {
                    println!(
                        "Execution {id} is already {}; nothing to cancel.",
                        status.as_str()
                    );
                }
                CancelOutcome::NotFound => {
                    anyhow::bail!("No durable execution {id} found.");
                }
                CancelOutcome::LiveOwner { pid } => {
                    anyhow::bail!(
                        "Execution {id} is currently locked by pid {pid} (an active owner, or a \
                         maintenance sweep/prune); live cancellation is not yet supported. If \
                         that process is the owner, stop it (or wait for it to exit) then re-run \
                         cancel; if it was a transient sweep, just retry shortly."
                    );
                }
                CancelOutcome::LivenessUnverifiable => {
                    anyhow::bail!(
                        "Cannot verify whether execution {id} has a live owner on this backend \
                         (no advisory lock available); refusing to cancel to avoid stopping a \
                         running execution unsafely."
                    );
                }
            }
        }

        DurableCommand::RotateKey {
            dry_run,
            drop_previous,
            force,
            ack_shared_db_drain,
        } => {
            handle_rotate_key(
                &config_file,
                &config,
                RotateKeyOptions {
                    dry_run,
                    drop_previous,
                    force,
                    ack_shared_db_drain,
                },
            )
            .await?;
        }
    }

    Ok(())
}

/// Flags controlling `zeph durable rotate-key`'s behavior, bundled into one struct rather than
/// four positional `bool` parameters (past clippy's `fn_params_excessive_bools` threshold, and
/// call sites read as named fields instead of an ambiguous run of `bool`s).
///
/// The four flags are independent CLI switches (mirroring `DurableConfig`'s own
/// `#[allow(clippy::struct_excessive_bools)]` precedent) — not state-machine variants, since any
/// combination is meaningful (e.g. `--dry-run --drop-previous --force`).
#[allow(clippy::struct_excessive_bools)]
struct RotateKeyOptions {
    /// Print intended changes without writing to the config file or vault.
    dry_run: bool,
    /// Close an open rotation window instead of opening a new one.
    drop_previous: bool,
    /// Skip the `--drop-previous` blob-scan (drop-previous only; never gates the open-window or
    /// shared-database refusals — see [`handle_open_window`]).
    force: bool,
    /// Acknowledge rotating on a declared/detected shared database despite the control-entry
    /// HMAC having no rotation window of its own.
    ack_shared_db_drain: bool,
}

/// Open or close a `ZEPH_DURABLE_KEY` AEAD rotation window (`zeph durable rotate-key`, #6447).
///
/// Dispatches to [`handle_open_window`] or [`handle_drop_previous`] after a partial-state (XOR)
/// detector that runs **before any write**, on both paths: if `config.durable.previous_key_id`
/// disagrees with whether `ZEPH_DURABLE_KEY_PREVIOUS` exists in the vault, this refuses rather
/// than guessing which side is stale — the state is only reachable by a crash mid-write or manual
/// editing, and deriving "the old key" from whatever `ZEPH_DURABLE_KEY` currently holds would
/// compound the inconsistency instead of resolving it.
///
/// # Errors
///
/// Returns an error if the vault cannot be loaded, the rotation state is inconsistent, or the
/// dispatched handler fails (see [`handle_open_window`] / [`handle_drop_previous`]).
async fn handle_rotate_key(
    config_file: &Path,
    config: &Config,
    opts: RotateKeyOptions,
) -> anyhow::Result<()> {
    let dir = zeph_core::vault::default_vault_dir();
    let key_path = dir.join("vault-key.txt");
    let vault_path = dir.join("secrets.age");
    if !key_path.exists() || !vault_path.exists() {
        anyhow::bail!(
            "no age vault found at {}; run `zeph --init` first to generate \
             {CURRENT_KEY_VAULT_NAME}",
            dir.display()
        );
    }
    let mut provider = AgeVaultProvider::load(&key_path, &vault_path)
        .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;

    // Partial-state (XOR) detector — MANDATORY, before any write. See the doc comment above.
    let window_declared = config.durable.previous_key_id.is_some();
    let prev_secret_present = provider.get(PREVIOUS_KEY_VAULT_NAME).is_some();
    if window_declared != prev_secret_present {
        anyhow::bail!(
            "inconsistent rotation state detected -- refusing to proceed.\n\
             config:  [durable] previous_key_id = {:?}\n\
             vault:   {PREVIOUS_KEY_VAULT_NAME} is {}\n\
             \n\
             This usually means a crash interrupted a previous `zeph durable rotate-key` run \
             between its config write and its vault write, or one side was hand-edited. Manual \
             recovery is required before retrying -- never derive the old key from whatever \
             {CURRENT_KEY_VAULT_NAME} currently holds:\n\
             - If {PREVIOUS_KEY_VAULT_NAME} is absent, {CURRENT_KEY_VAULT_NAME} in the vault \
               still holds the pre-rotation key: edit [durable] in {} and remove \
               `previous_key_id` (restoring `key_id` to its prior value), then retry.\n\
             - If {PREVIOUS_KEY_VAULT_NAME} is present but config has no `previous_key_id`, \
               either restore `previous_key_id` in the config to match, or remove the orphaned \
               vault secret (`zeph vault rm {PREVIOUS_KEY_VAULT_NAME}`) once you have confirmed \
               it is not needed.",
            config.durable.previous_key_id,
            if prev_secret_present {
                "present"
            } else {
                "absent"
            },
            config_file.display(),
        );
    }

    if opts.drop_previous {
        handle_drop_previous(config_file, config, &mut provider, opts.dry_run, opts.force).await
    } else {
        handle_open_window(
            config_file,
            config,
            &mut provider,
            opts.dry_run,
            opts.ack_shared_db_drain,
        )
    }
}

/// Open a new rotation window: generate a fresh `ZEPH_DURABLE_KEY`, stash the old key under
/// `ZEPH_DURABLE_KEY_PREVIOUS`, and bump `[durable] key_id`/`previous_key_id`.
///
/// Write order is **config first, then vault** (R1): a crash between the two leaves config
/// declaring a window that the vault does not yet back, which [`load_durable_cipher`]'s
/// fail-closed branch turns into a loud startup error instead of a silent mis-decrypt. Refuses a
/// second window while one is already open (R2 — the cipher holds only one previous key slot, so
/// a second rotation would silently orphan the first previous key) and refuses on a
/// declared/detected shared database without `--ack-shared-db-drain` (R3 — the control-entry HMAC
/// key has no rotation window). Neither refusal is bypassable by any flag.
fn handle_open_window(
    config_file: &Path,
    config: &Config,
    provider: &mut AgeVaultProvider,
    dry_run: bool,
    ack_shared_db_drain: bool,
) -> anyhow::Result<()> {
    // R2 — refuse to open a second window; the cipher's single previous-key slot cannot hold two.
    if let Some(prev_id) = config.durable.previous_key_id {
        anyhow::bail!(
            "a rotation window is already open (previous_key_id = {prev_id}); close it with \
             `zeph durable rotate-key --drop-previous` after the retention window has elapsed, \
             then rotate again"
        );
    }

    // R3 — shared-DB guard, enforced (not merely documented). Never bypassable by --force.
    let url = resolve_durable_db_url(config);
    if is_shared_db(&config.durable, &url) && !ack_shared_db_drain {
        let msg = "this durable journal is a shared database. Rotating ZEPH_DURABLE_KEY \
             re-derives the control-entry HMAC key, which has no rotation window of its own -- \
             every in-flight execution's control entries will fail ControlIntegrity until they \
             drain. Drain (finalize/prune) all in-flight executions first, then re-run with \
             --ack-shared-db-drain. (HMAC-key rotation window: follow-up to #6447.)";
        if dry_run {
            println!("DRY RUN: {msg}");
            return Ok(());
        }
        anyhow::bail!("{msg}");
    }

    let Some(old_key_b64) = provider.get(CURRENT_KEY_VAULT_NAME).map(str::to_owned) else {
        anyhow::bail!("{CURRENT_KEY_VAULT_NAME} not found in vault; nothing to rotate");
    };

    let old_key_id = config.durable.key_id;
    let new_key_id = old_key_id.wrapping_add(1);

    let vault_path = zeph_core::vault::default_vault_dir().join("secrets.age");
    if dry_run {
        println!(
            "DRY RUN -- no changes written.\n\
             Would rotate ZEPH_DURABLE_KEY: key_id {old_key_id} -> {new_key_id}\n\
             Would set [durable] key_id = {new_key_id}, previous_key_id = {old_key_id} in {}\n\
             Would generate a new {CURRENT_KEY_VAULT_NAME} and stash the old key under \
             {PREVIOUS_KEY_VAULT_NAME} in {}\n\
             A process restart is required to pick up the rotated key (no hot-reload).",
            config_file.display(),
            vault_path.display(),
        );
        return Ok(());
    }

    let new_key_b64 = zeph_core::durable::generate_durable_key_b64();

    // R1 — config FIRST, then vault. A crash between the two fires load_durable_cipher's
    // fail-closed branch (loud, no silent mis-decrypt) rather than mis-decrypting silently.
    write_durable_config_fields(config_file, new_key_id, Some(old_key_id))?;

    provider
        .set_secret_mut(PREVIOUS_KEY_VAULT_NAME.to_owned(), old_key_b64, true)
        .map_err(|e| anyhow::anyhow!("failed to stash previous key in vault: {e}"))?;
    provider
        .set_secret_mut(CURRENT_KEY_VAULT_NAME.to_owned(), new_key_b64, true)
        .map_err(|e| anyhow::anyhow!("failed to set new {CURRENT_KEY_VAULT_NAME}: {e}"))?;
    provider
        .save()
        .map_err(|e| anyhow::anyhow!("failed to save vault: {e}"))?;

    println!(
        "Rotated ZEPH_DURABLE_KEY: key_id {old_key_id} -> {new_key_id}.\n\
         The previous key (id {old_key_id}) remains readable via the rotation window until you \
         run `zeph durable rotate-key --drop-previous`.\n\
         Config updated: {}\n\
         Vault updated: {}\n\
         A process restart is required to pick up the rotated key -- the cipher is built once \
         at startup and does not hot-reload.",
        config_file.display(),
        vault_path.display(),
    );
    Ok(())
}

/// Close an open rotation window: verify no sealed payload still uses the previous key, then
/// remove `ZEPH_DURABLE_KEY_PREVIOUS` from the vault and clear `previous_key_id`.
///
/// The blob-scan verification is default-on (R4): refuses the drop when
/// [`LocalBackend::count_sealed_under_key_id`] finds any surviving row, since dropping the
/// previous key then makes those blobs permanently unreadable. `--force` skips the scan for an
/// operator who has independently confirmed pruning is complete; it applies **only** to this
/// scan, never to the caller's partial-state detector or [`handle_open_window`]'s refusals. A
/// call with no window open (`previous_key_id = None`, `ZEPH_DURABLE_KEY_PREVIOUS` absent — the
/// caller's partial-state detector already guarantees these agree) is a clean informational
/// no-op, not an error.
async fn handle_drop_previous(
    config_file: &Path,
    config: &Config,
    provider: &mut AgeVaultProvider,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<()> {
    let Some(previous_key_id) = config.durable.previous_key_id else {
        println!("No rotation window is open; nothing to drop.");
        return Ok(());
    };

    if !force {
        let url = resolve_durable_db_url(config);
        if url != ":memory:" && Path::new(&url).exists() {
            let backend = LocalBackend::open(&url, config.durable.max_payload_bytes)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open durable journal: {e}"))?;
            let matches = backend
                .count_sealed_under_key_id(previous_key_id)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("failed to scan journal for key-id {previous_key_id}: {e}")
                })?;
            if matches > 0 {
                anyhow::bail!(
                    "refusing to drop: {matches} sealed payload(s) in {url} still tagged \
                     key_id = {previous_key_id}. Dropping the previous key now would make them \
                     permanently unreadable (UnknownKeyId). Wait for retention to prune them, or \
                     pass --force to skip this scan if you have independently confirmed pruning \
                     is complete (note: a deployment with plaintext-mode rows -- \
                     encrypt_payload = false -- can occasionally over-count a coincidental \
                     leading byte match; that is fail-safe, never a missed match)."
                );
            }
        }
    }

    let vault_path = zeph_core::vault::default_vault_dir().join("secrets.age");
    if dry_run {
        println!(
            "DRY RUN -- no changes written.\n\
             Would remove {PREVIOUS_KEY_VAULT_NAME} from {} and clear previous_key_id \
             (currently {previous_key_id}) from {}.",
            vault_path.display(),
            config_file.display(),
        );
        return Ok(());
    }

    write_durable_config_fields(config_file, config.durable.key_id, None)?;

    provider.remove_secret_mut(PREVIOUS_KEY_VAULT_NAME);
    provider
        .save()
        .map_err(|e| anyhow::anyhow!("failed to save vault: {e}"))?;

    println!(
        "Rotation window closed: removed {PREVIOUS_KEY_VAULT_NAME} and cleared previous_key_id \
         (was {previous_key_id}). Payloads still sealed under key_id {previous_key_id} are now \
         permanently unreadable.\n\
         Config updated: {}\n\
         Vault updated: {}",
        config_file.display(),
        vault_path.display(),
    );
    Ok(())
}

/// Surgically set `[durable] key_id` and `[durable] previous_key_id` in the on-disk config file
/// at `config_file`, preserving all other content and formatting.
///
/// Uses `toml_edit::DocumentMut` rather than a full `Config` re-serialize (which would discard
/// user comments/formatting) — the same rationale as the raw-text migration passes in
/// `zeph-config`, just scoped to a single table here. Written atomically via
/// [`crate::commands::migrate::atomic_write`].
///
/// # Errors
///
/// Returns an error if the file cannot be read/parsed as TOML, `[durable]` exists but is not a
/// table, or the atomic write fails.
fn write_durable_config_fields(
    config_file: &Path,
    key_id: u8,
    previous_key_id: Option<u8>,
) -> anyhow::Result<()> {
    let raw = if config_file.exists() {
        std::fs::read_to_string(config_file)
            .with_context(|| format!("failed to read {}", config_file.display()))?
    } else {
        String::new()
    };
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_file.display()))?;

    let durable_item = doc
        .entry("durable")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let durable_table = durable_item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[durable] is not a table in {}", config_file.display()))?;

    durable_table["key_id"] = toml_edit::value(i64::from(key_id));
    match previous_key_id {
        Some(id) => {
            durable_table["previous_key_id"] = toml_edit::value(i64::from(id));
        }
        None => {
            durable_table.remove("previous_key_id");
        }
    }

    crate::commands::migrate::atomic_write(config_file, &doc.to_string())
}

/// Map an opaque "unknown cipher key-id" [`zeph_durable::DurableError`] from a `--reveal` read to
/// an actionable message, instead of surfacing the raw decode-failure string.
///
/// A blob whose leading key-id byte the current in-memory cipher does not recognize almost
/// always means the process was never restarted after a `zeph durable rotate-key` rotation (the
/// cipher is built once at startup, R5 / #6447) — restarting picks up the rotated
/// `ZEPH_DURABLE_KEY`/`ZEPH_DURABLE_KEY_PREVIOUS` pair. Every other error passes through
/// unchanged.
fn describe_reveal_error(e: &zeph_durable::DurableError) -> String {
    if matches!(
        e,
        zeph_durable::DurableError::Decode {
            context: "unknown cipher key-id"
        }
    ) {
        format!(
            "{e} -- this blob's key-id predates a key rotation the current cipher does not \
             recognize. Restart the process to pick up the rotated ZEPH_DURABLE_KEY / \
             ZEPH_DURABLE_KEY_PREVIOUS pair, or check `zeph durable rotate-key`'s rotation \
             window is still open (`--drop-previous` closes it permanently)."
        )
    } else {
        e.to_string()
    }
}

/// Show one execution's entries, optionally filtered to a single `step`, redacted unless `reveal`.
///
/// `reveal` reads through the AEAD cipher (decrypted payloads) and prints a warning first; otherwise
/// only redaction-safe metadata is shown (INV-5). When `json` is set, output is serialized JSON
/// instead of a human-readable table.
///
/// # Errors
///
/// Returns an error if the journal read fails.
async fn show_entries(
    backend: &LocalBackend,
    exec: ExecutionId,
    reveal: bool,
    step: Option<u32>,
    json: bool,
) -> anyhow::Result<()> {
    if reveal {
        println!("{REVEAL_WARNING}\n");
        let mut entries = backend.read_execution(exec).await.map_err(|e| {
            anyhow::anyhow!("failed to read execution: {}", describe_reveal_error(&e))
        })?;
        if let Some(s) = step {
            entries.retain(|e| e.step_id.value() == s);
        }
        if entries.is_empty() {
            println!("No matching journal entry.");
        } else {
            print_revealed(&entries);
        }
    } else {
        let mut entries = backend
            .read_execution_redacted(exec)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read execution: {e}"))?;
        if let Some(s) = step {
            entries.retain(|e| e.step_id.value() == s);
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&entries)
                    .context("failed to serialize journal entries")?
            );
        } else if entries.is_empty() {
            println!("No matching journal entry.");
        } else {
            print_redacted(&entries);
        }
    }
    Ok(())
}

/// Print redaction-safe entry metadata (default output, INV-5).
fn print_redacted(entries: &[zeph_durable::RedactedEntry]) {
    if entries.is_empty() {
        println!("No journal entries.");
        return;
    }
    println!(
        "{:>6} {:>6} {:<14} {:<20} {:>10}  {:<18} CREATED",
        "SEQ", "STEP", "ENTRY KIND", "EFFECT CLASS", "BYTES", "IDEM KEY"
    );
    println!("{}", "-".repeat(96));
    for e in entries {
        println!(
            "{:>6} {:>6} {:<14} {:<20} {:>10}  {:<18} {}",
            e.seq,
            e.step_id.value(),
            e.entry_kind,
            e.effect_class.as_deref().unwrap_or("-"),
            e.payload_len,
            e.idem_key_prefix.as_deref().unwrap_or("-"),
            fmt_ts(e.created_at_ms),
        );
    }
}

/// Print decrypted entry payloads (`--reveal` only).
fn print_revealed(entries: &[zeph_durable::JournalEntry]) {
    use zeph_durable::EntryKind;
    if entries.is_empty() {
        println!("No journal entries.");
        return;
    }
    for e in entries {
        let step = e.step_id.value();
        match &e.entry {
            EntryKind::StepResult {
                payload, effect, ..
            } => {
                println!(
                    "step {step} [step_result, {effect:?}] {} bytes:",
                    payload.len()
                );
                println!("  {}", String::from_utf8_lossy(payload));
            }
            other => {
                println!("step {step} [{}] (no payload)", other.tag());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn durable_db_is_filename_namespaced_sibling_of_sqlite_path() {
        let mut config = Config::default();
        config.memory.sqlite_path = "/data/zeph/zeph.db".to_owned();
        assert_eq!(
            resolve_durable_db_url(&config),
            "/data/zeph/zeph.db.durable.db"
        );
    }

    /// Regression for #5553: distinct memory databases sharing a directory must resolve to
    /// distinct journal files, or their fresh conversations collide on `ExecutionId`.
    #[test]
    fn distinct_sqlite_stems_resolve_to_distinct_durable_urls() {
        let dir = tempfile::tempdir().unwrap();
        let mut config_a = Config::default();
        config_a.memory.sqlite_path = dir.path().join("alpha.db").to_string_lossy().into_owned();
        let mut config_b = Config::default();
        config_b.memory.sqlite_path = dir.path().join("beta.db").to_string_lossy().into_owned();

        assert_ne!(
            resolve_durable_db_url(&config_a),
            resolve_durable_db_url(&config_b)
        );
    }

    /// Regression for critic finding F1 on #5553: `file_stem()`-only namespacing would collapse
    /// same-stem, different-extension DBs (e.g. `zeph.db` and `zeph.sqlite`) onto the same journal
    /// file, reproducing the exact shared-journal condition the fix is meant to close.
    #[test]
    fn same_stem_different_extension_resolves_to_distinct_durable_urls() {
        let dir = tempfile::tempdir().unwrap();
        let mut config_a = Config::default();
        config_a.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        let mut config_b = Config::default();
        config_b.memory.sqlite_path = dir
            .path()
            .join("zeph.sqlite")
            .to_string_lossy()
            .into_owned();

        assert_ne!(
            resolve_durable_db_url(&config_a),
            resolve_durable_db_url(&config_b)
        );
    }

    /// Regression for #5553: an already-existing bare `durable.db` (the pre-fix layout) must
    /// keep resolving to itself so upgrading deployments do not orphan their journal history.
    #[test]
    fn preexisting_legacy_durable_db_is_preferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("durable.db"), []).unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();

        assert_eq!(
            resolve_durable_db_url(&config),
            dir.path().join("durable.db").to_string_lossy()
        );
    }

    /// End-to-end regression for #5553: two distinct `memory.sqlite_path` databases in the same
    /// directory, each opening its P1 durable backend for its first-ever conversation, must land
    /// in genuinely distinct journal files on disk and must not collide on `ExecutionId` — the
    /// full composition of `resolve_durable_db_url` (file separation) and the `ExecutionId`
    /// derivation fold (defense in depth), exercised against real `LocalBackend` instances rather
    /// than `:memory:` stand-ins.
    #[tokio::test]
    async fn distinct_databases_do_not_collide_on_shared_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sqlite_a = dir.path().join("alpha.db").to_string_lossy().into_owned();
        let sqlite_b = dir.path().join("beta.db").to_string_lossy().into_owned();

        let mut config_a = Config::default();
        config_a.memory.sqlite_path = sqlite_a.clone();
        let mut config_b = Config::default();
        config_b.memory.sqlite_path = sqlite_b.clone();

        let url_a = resolve_durable_db_url(&config_a);
        let url_b = resolve_durable_db_url(&config_b);
        assert_ne!(
            url_a, url_b,
            "the two databases must resolve to distinct journal files"
        );

        let backend_a = LocalBackend::open(&url_a, 1_000_000).await.unwrap();
        backend_a.init().await.unwrap();
        let backend_b = LocalBackend::open(&url_b, 1_000_000).await.unwrap();
        backend_b.init().await.unwrap();

        assert!(
            Path::new(&url_a).exists() && Path::new(&url_b).exists(),
            "both journal files must actually exist on disk"
        );
        assert_ne!(
            std::fs::canonicalize(&url_a).unwrap(),
            std::fs::canonicalize(&url_b).unwrap(),
            "the two journal files must be genuinely distinct files, not the same file via aliasing"
        );

        // Mirrors the P1 fold in `ensure_session_durable_ctx`: ConversationId(1)'s fixed-width
        // bytes plus the owning database's `sqlite_path`.
        let conv_one = 1u64.to_le_bytes();
        let fold = |sqlite_path: &str| {
            let mut payload = conv_one.to_vec();
            payload.extend_from_slice(sqlite_path.as_bytes());
            ExecutionId::derive(b"zeph.agent_turn.v1", &payload)
        };
        let exec_a = fold(&sqlite_a);
        let exec_b = fold(&sqlite_b);
        assert_ne!(exec_a, exec_b);

        let resumed_a = backend_a
            .open_execution(exec_a, zeph_durable::ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let resumed_b = backend_b
            .open_execution(exec_b, zeph_durable::ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(
            !resumed_a && !resumed_b,
            "each database's first conversation must open a fresh execution, not resume the other's"
        );

        let executions_a = backend_a.list_executions(None, None, 10).await.unwrap();
        let executions_b = backend_b.list_executions(None, None, 10).await.unwrap();
        assert_eq!(executions_a.len(), 1);
        assert_eq!(executions_b.len(), 1);
        assert_eq!(executions_a[0].execution_id, exec_a);
        assert_eq!(executions_b[0].execution_id, exec_b);
    }

    #[test]
    fn durable_db_bare_filename_when_path_has_no_parent() {
        let mut config = Config::default();
        config.memory.sqlite_path = "zeph.db".to_owned();
        assert_eq!(resolve_durable_db_url(&config), "durable.db");
    }

    #[test]
    fn fmt_ts_formats_epoch_millis_as_utc() {
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00");
    }

    /// Regression for #5404: `--reveal` must not require `ZEPH_DURABLE_KEY` when
    /// `encrypt_payload = false` — stored payloads are already plaintext.
    #[tokio::test]
    async fn open_backend_reveal_succeeds_without_key_when_encrypt_payload_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.encrypt_payload = false;

        let url = resolve_durable_db_url(&config);
        std::fs::write(&url, []).unwrap();

        let backend = open_backend(&config, true)
            .await
            .expect("--reveal must succeed without ZEPH_DURABLE_KEY when encrypt_payload=false");
        assert!(backend.is_some());
    }

    /// Regression for #5996 (critic finding S2): `open_backend` — the `zeph durable`
    /// CLI read path shared by `list`/`show`/`inspect`/`prune`/`resume`/`--reveal` — must reject
    /// `encrypt_payload = false` combined with a declared `shared_db = true`, the same INV-8
    /// forbidden combination `load_write_cipher` rejects on the write path. Without this test the
    /// `enforce_encryption_gate(&config.durable, &url)?` line in `open_backend` could be silently
    /// deleted with the rest of the suite still green.
    #[tokio::test]
    async fn open_backend_rejects_disabled_encryption_when_shared_db_declared() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.encrypt_payload = false;
        config.durable.shared_db = true;

        let url = resolve_durable_db_url(&config);
        std::fs::write(&url, []).unwrap();

        let result = open_backend(&config, false).await;
        assert!(
            result.is_err(),
            "open_backend must fail closed for encrypt_payload=false on a declared shared_db (INV-8)"
        );

        // Also rejected on the --reveal path, before any decryption is even attempted.
        let reveal_result = open_backend(&config, true).await;
        assert!(
            reveal_result.is_err(),
            "open_backend --reveal must fail closed for the same forbidden combination"
        );
    }

    /// Regression for #5404: `--reveal` must still require `ZEPH_DURABLE_KEY` when
    /// `encrypt_payload = true` (the default, encrypted-at-rest posture).
    #[allow(unsafe_code)]
    #[tokio::test]
    #[serial]
    async fn open_backend_reveal_requires_key_when_encrypt_payload_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.encrypt_payload = true;

        let url = resolve_durable_db_url(&config);
        std::fs::write(&url, []).unwrap();

        // Point the vault dir at an empty temp dir so no real ZEPH_DURABLE_KEY is found,
        // regardless of what happens to be configured on the machine running this test.
        let vault_dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", vault_dir.path());
        }

        let result = open_backend(&config, true).await;

        unsafe {
            match &prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            result.is_err(),
            "expected --reveal to fail without ZEPH_DURABLE_KEY when encrypt_payload=true"
        );
    }

    /// End-to-end regression for #5414: `encrypt_payload = true` must actually attach a cipher
    /// on the write path (`load_write_cipher`, consumed by `runner.rs` and
    /// `scheduler_daemon.rs`), not just gate `--reveal` on the read path. Writes a step result
    /// through the real backend using the cipher `load_write_cipher` resolves from a real vault
    /// key, then reads the raw `durable_journal.payload` column directly (bypassing decryption)
    /// and asserts it is neither the plaintext bytes nor valid JSON — i.e., genuinely ciphertext
    /// at rest, mirroring the issue's reproduction steps.
    #[allow(unsafe_code)]
    #[tokio::test]
    #[serial]
    async fn write_path_attaches_cipher_and_seals_payload_when_encrypt_payload_enabled() {
        use zeph_durable::{
            EffectClass, EntryKind, ExecutionKind, IdempotencyKey, JournalEntry, StepId,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.encrypt_payload = true;

        // Point the vault dir at a fresh temp dir and seed a real ZEPH_DURABLE_KEY, mirroring
        // what `zeph --init` does (src/init/durable.rs::store_durable_key).
        let vault_dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", vault_dir.path());
        }

        let vault_root = zeph_core::vault::default_vault_dir();
        zeph_core::vault::AgeVaultProvider::init_vault(&vault_root).unwrap();
        let mut provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        provider
            .set_secret_mut(
                "ZEPH_DURABLE_KEY".to_owned(),
                zeph_core::durable::generate_durable_key_b64(),
                false,
            )
            .unwrap();
        provider.save().unwrap();

        // Exercise the exact glue the write paths (runner.rs, scheduler_daemon.rs) call.
        let cipher = load_write_cipher(&config)
            .expect("cipher load must succeed with a real vault key")
            .expect("encrypt_payload=true must produce a cipher");

        let url = resolve_durable_db_url(&config);
        let backend = LocalBackend::open(&url, config.durable.max_payload_bytes)
            .await
            .unwrap()
            .with_cipher(cipher);
        backend.init().await.unwrap();

        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let step_id = StepId::new(0);
        let plaintext: &[u8] = br#"{"secret":"token-value"}"#;
        backend
            .append(JournalEntry {
                seq: None,
                execution_id: exec,
                kind: ExecutionKind::AgentTurn,
                step_id,
                entry: EntryKind::StepResult {
                    idempotency_key: IdempotencyKey::derive(exec, step_id, b"tool:test"),
                    payload: bytes::Bytes::copy_from_slice(plaintext),
                    effect: EffectClass::Idempotent,
                    payload_version: 1,
                },
                created_at_ms: 0,
            })
            .await
            .unwrap();

        let (stored,): (Option<Vec<u8>>,) = zeph_db::query_as(zeph_db::sql!(
            "SELECT payload FROM durable_journal WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        let stored = stored.expect("payload present");

        unsafe {
            match &prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert_ne!(
            stored.as_slice(),
            plaintext,
            "payload must not be stored verbatim when encrypt_payload=true"
        );
        assert!(
            serde_json::from_slice::<serde_json::Value>(&stored).is_err(),
            "sealed payload must not parse as plaintext JSON"
        );

        // Round-trips back to plaintext through the same cipher-attached backend.
        let entries = backend.read_execution(exec).await.unwrap();
        match &entries[0].entry {
            EntryKind::StepResult { payload, .. } => assert_eq!(payload.as_ref(), plaintext),
            other => panic!("unexpected entry kind: {other:?}"),
        }
    }

    /// Regression for #5996: `encrypt_payload = false` combined with a declared `shared_db =
    /// true` must fail closed via the INV-8 `encryption_gate`, instead of silently persisting
    /// plaintext journal payloads to a multi-client database.
    #[tokio::test]
    async fn load_write_cipher_rejects_disabled_encryption_when_shared_db_declared() {
        let mut config = Config::default();
        config.durable.encrypt_payload = false;
        config.durable.shared_db = true;
        assert!(
            load_write_cipher(&config).is_err(),
            "encrypt_payload=false on a declared shared_db must fail closed (INV-8)"
        );
    }

    /// Regression for #5996: the documented dev-only override (`encrypt_payload = false` on an
    /// ordinary single-user local, non-shared database) must still succeed — the gate only warns.
    #[tokio::test]
    async fn load_write_cipher_warns_but_succeeds_for_undeclared_local_override() {
        let mut config = Config::default();
        config.durable.encrypt_payload = false;
        // shared_db left at its default (false) - the permitted dev-only override.
        assert!(load_write_cipher(&config).unwrap().is_none());
    }

    /// Regression for #5996: `is_shared_db` recognizes a `postgres://`/`postgresql://` URL scheme
    /// as shared-by-construction even when the operator has not (yet) set `shared_db = true` —
    /// defense in depth for a future Postgres-backed `resolve_durable_db_url`.
    #[test]
    fn is_shared_db_detects_postgres_url_scheme_even_when_flag_unset() {
        let config = zeph_core::config::DurableConfig::default();
        assert!(!config.shared_db);
        assert!(is_shared_db(&config, "postgres://user@host/db"));
        assert!(is_shared_db(&config, "postgresql://user@host/db"));
        assert!(!is_shared_db(&config, "/local/path/durable.db"));
    }

    /// Regression for #5996: a `postgres://`-scheme URL combined with `encrypt_payload = false`
    /// must fail closed via `load_write_cipher` even though `shared_db` was never explicitly set
    /// — the URL-scheme detection in [`is_shared_db`] is defense in depth, not the primary signal.
    #[tokio::test]
    async fn load_write_cipher_rejects_disabled_encryption_for_postgres_url_scheme() {
        let mut config = Config::default();
        config.durable.encrypt_payload = false;
        // memory.sqlite_path drives resolve_durable_db_url; a `postgres://`-looking value
        // exercises the URL-scheme branch of `is_shared_db` without needing a real Postgres build.
        config.memory.sqlite_path = "postgres://user@host/db".to_owned();
        assert!(
            load_write_cipher(&config).is_err(),
            "a postgres:// resolved URL must be treated as shared even without shared_db=true"
        );
    }

    /// Regression for #6043/#6044: a single-user local, non-shared database must never resolve a
    /// control-entry HMAC key — the documented INV-8 stance where such deployments' control
    /// entries carry no HMAC. No vault access should even be attempted.
    #[tokio::test]
    async fn load_write_hmac_key_returns_none_for_single_user_local() {
        let config = Config::default();
        assert!(!config.durable.shared_db);
        assert!(
            load_write_hmac_key(&config).unwrap().is_none(),
            "single-user local, non-shared database must not resolve an HMAC key"
        );
    }

    /// Regression for #6043/#6044: a declared shared database must fail closed when
    /// `ZEPH_DURABLE_KEY` cannot be resolved from the vault, mirroring
    /// `open_backend_reveal_requires_key_when_encrypt_payload_enabled`'s pattern for the AEAD key.
    #[allow(unsafe_code)]
    #[tokio::test]
    #[serial]
    async fn load_write_hmac_key_fails_closed_when_shared_db_declared_and_key_missing() {
        let mut config = Config::default();
        config.durable.shared_db = true;

        let vault_dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", vault_dir.path());
        }

        let result = load_write_hmac_key(&config);

        unsafe {
            match &prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            result.is_err(),
            "a declared shared database must fail closed without ZEPH_DURABLE_KEY (INV-8)"
        );
    }

    /// Write a minimal config file (default config with `memory.sqlite_path` overridden) and
    /// return its path, for exercising [`handle_durable_command`]'s config-file-driven entry
    /// point rather than the lower-level `open_backend` helper directly.
    ///
    /// Also touches an empty durable journal file at the resolved URL — `open_backend` treats a
    /// missing file as "durable execution has never run yet" and refuses to open it (prints a
    /// friendly notice and returns `Ok(None)`), so a fresh temp dir must have the file pre-created
    /// before any `open_backend`/`handle_durable_command` call, mirroring
    /// `open_backend_reveal_succeeds_without_key_when_encrypt_payload_disabled`'s pattern above.
    fn write_config_toml(dir: &std::path::Path) -> std::path::PathBuf {
        let mut config = Config::default();
        config.memory.sqlite_path = dir.join("zeph.db").to_string_lossy().into_owned();
        let toml = toml::to_string_pretty(&config).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, toml).unwrap();
        std::fs::write(resolve_durable_db_url(&config), []).unwrap();
        path
    }

    /// Regression for #6362: `zeph durable cancel <id>` dispatches to `LocalBackend::cancel_execution`
    /// and actually marks the row `canceled` in the database.
    #[tokio::test]
    async fn handle_durable_command_cancel_marks_a_running_execution_canceled() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());
        let config = load_config_or_default(&config_path);

        let exec = ExecutionId::new();
        {
            let backend = open_backend(&config, false).await.unwrap().unwrap();
            backend
                .open_execution(exec, zeph_durable::ExecutionKind::AgentTurn)
                .await
                .unwrap();
        }

        let result = handle_durable_command(
            DurableCommand::Cancel {
                id: exec.as_uuid().to_string(),
            },
            Some(&config_path),
        )
        .await;
        assert!(
            result.is_ok(),
            "canceling a running execution must succeed: {result:?}"
        );

        let backend = open_backend(&config, false).await.unwrap().unwrap();
        let status = backend.execution_status(exec).await.unwrap();
        assert_eq!(status, Some(zeph_durable::ExecutionStatus::Canceled));
    }

    /// Regression for #6362 (FR-004): canceling an unknown execution id must exit non-zero.
    #[tokio::test]
    async fn handle_durable_command_cancel_on_unknown_id_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());

        let result = handle_durable_command(
            DurableCommand::Cancel {
                id: ExecutionId::new().as_uuid().to_string(),
            },
            Some(&config_path),
        )
        .await;
        assert!(
            result.is_err(),
            "canceling an unknown execution id must exit non-zero"
        );
    }

    /// Regression for #6362 (FR-011): `zeph durable resume` on a canceled execution must report
    /// the distinct "intentionally canceled" message and succeed, rather than reaching the
    /// generic "adapters not wired" message or erroring.
    #[tokio::test]
    async fn handle_durable_command_resume_on_canceled_execution_refuses_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());
        let config = load_config_or_default(&config_path);

        let exec = ExecutionId::new();
        {
            let backend = open_backend(&config, false).await.unwrap().unwrap();
            backend
                .open_execution(exec, zeph_durable::ExecutionKind::AgentTurn)
                .await
                .unwrap();
            let outcome = backend.cancel_execution(exec).await.unwrap();
            assert_eq!(outcome, CancelOutcome::Canceled);
        }

        let result = handle_durable_command(
            DurableCommand::Resume {
                id: exec.as_uuid().to_string(),
            },
            Some(&config_path),
        )
        .await;
        assert!(
            result.is_ok(),
            "resume on a canceled execution must report a message and exit 0, not error: {result:?}"
        );
    }

    /// Regression for #6043/#6044: a declared shared database resolves a real 32-byte HMAC key
    /// from a seeded vault, mirroring
    /// `write_path_attaches_cipher_and_seals_payload_when_encrypt_payload_enabled`'s pattern for
    /// the AEAD cipher — this exercises the exact glue `runner.rs` and `scheduler_daemon.rs` call.
    #[allow(unsafe_code)]
    #[tokio::test]
    #[serial]
    async fn load_write_hmac_key_resolves_real_key_when_shared_db_declared() {
        let mut config = Config::default();
        config.durable.shared_db = true;

        let vault_dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", vault_dir.path());
        }

        let vault_root = zeph_core::vault::default_vault_dir();
        zeph_core::vault::AgeVaultProvider::init_vault(&vault_root).unwrap();
        let mut provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        provider
            .set_secret_mut(
                "ZEPH_DURABLE_KEY".to_owned(),
                zeph_core::durable::generate_durable_key_b64(),
                false,
            )
            .unwrap();
        provider.save().unwrap();

        let result = load_write_hmac_key(&config);

        unsafe {
            match &prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            result.unwrap().is_some(),
            "a declared shared database with a real vault key must resolve an HMAC key"
        );
    }

    // ── `zeph durable rotate-key` (#6447) ──────────────────────────────────────────────────

    /// RAII guard mirroring `VaultDirGuard` in `src/init/durable.rs` tests: points
    /// `zeph_core::vault::default_vault_dir()` at a fresh temp dir for the test's duration via
    /// `XDG_CONFIG_HOME`, restoring the prior value on drop. Callers must be `#[serial]` since
    /// `XDG_CONFIG_HOME` is process-global.
    #[allow(unsafe_code)]
    struct VaultDirGuard {
        _dir: tempfile::TempDir,
        prev_xdg: Option<String>,
    }

    #[allow(unsafe_code)]
    impl VaultDirGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", dir.path());
            }
            Self {
                _dir: dir,
                prev_xdg,
            }
        }
    }

    #[allow(unsafe_code)]
    impl Drop for VaultDirGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_xdg {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    /// Initialize the age vault (assumes [`VaultDirGuard`] is already in scope) and seed
    /// `ZEPH_DURABLE_KEY`, returning the generated key.
    fn seed_vault_with_durable_key() -> String {
        let vault_root = zeph_core::vault::default_vault_dir();
        zeph_core::vault::AgeVaultProvider::init_vault(&vault_root).unwrap();
        let mut provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        let key = zeph_core::durable::generate_durable_key_b64();
        provider
            .set_secret_mut(CURRENT_KEY_VAULT_NAME.to_owned(), key.clone(), false)
            .unwrap();
        provider.save().unwrap();
        key
    }

    /// Happy path: `rotate-key` bumps `[durable] key_id`/`previous_key_id` in the config file and
    /// stashes the old key under `ZEPH_DURABLE_KEY_PREVIOUS` in the vault, without ever printing
    /// raw key bytes (not directly assertable here, but the handler only ever interpolates
    /// key-ids and paths into its output strings — see `handle_open_window`).
    #[tokio::test]
    #[serial]
    async fn rotate_key_opens_a_window_and_updates_config_and_vault() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());

        let result = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(result.is_ok(), "rotate-key must succeed: {result:?}");

        let config = load_config_or_default(&config_path);
        assert_eq!(config.durable.key_id, 1);
        assert_eq!(config.durable.previous_key_id, Some(0));

        let vault_root = zeph_core::vault::default_vault_dir();
        let provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        assert!(
            provider.get(PREVIOUS_KEY_VAULT_NAME).is_some(),
            "the old key must be stashed under ZEPH_DURABLE_KEY_PREVIOUS"
        );
    }

    /// R2: a second rotation while a window is already open must be refused, and must not touch
    /// config or vault (the first previous key stays intact).
    #[tokio::test]
    #[serial]
    async fn rotate_key_refuses_a_second_window() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());

        handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await
        .unwrap();
        let after_first = load_config_or_default(&config_path);
        assert_eq!(after_first.durable.previous_key_id, Some(0));

        let result = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(
            result.is_err(),
            "a second rotation while a window is open must be refused"
        );

        let after_second = load_config_or_default(&config_path);
        assert_eq!(
            after_second.durable, after_first.durable,
            "the refused second rotation must not mutate config"
        );
    }

    /// `--dry-run` must never write to the config file or the vault.
    #[tokio::test]
    #[serial]
    async fn rotate_key_dry_run_writes_nothing() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());
        let before = std::fs::read_to_string(&config_path).unwrap();

        let result = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: true,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(result.is_ok());

        let after = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(before, after, "--dry-run must not modify the config file");

        let vault_root = zeph_core::vault::default_vault_dir();
        let provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        assert!(provider.get(PREVIOUS_KEY_VAULT_NAME).is_none());
    }

    /// A `--drop-previous` call with no rotation window open is a clean informational no-op.
    #[tokio::test]
    #[serial]
    async fn rotate_key_drop_previous_is_noop_when_no_window_open() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());
        let before = std::fs::read_to_string(&config_path).unwrap();

        let result = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: true,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(
            result.is_ok(),
            "no-window drop-previous must succeed: {result:?}"
        );

        let after = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            before, after,
            "a no-op drop-previous must not modify the config file"
        );
    }

    /// R4: `--drop-previous` refuses when a sealed payload still carries the previous key-id, and
    /// `--force` skips the scan and lets the drop proceed.
    #[tokio::test]
    #[serial]
    async fn rotate_key_drop_previous_refuses_with_surviving_blob_unless_forced() {
        use zeph_durable::{
            EffectClass, EntryKind, ExecutionKind, IdempotencyKey, JournalEntry, LocalBackend,
            StepId,
        };

        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());

        handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        // Seal a payload directly under key-id 0 (the now-previous key) by writing through a
        // no-cipher backend with a byte-0-prefixed plaintext payload — this exercises the scan
        // predicate itself without depending on the real AEAD cipher's on-disk framing.
        let config = load_config_or_default(&config_path);
        let url = resolve_durable_db_url(&config);
        let backend = LocalBackend::open(&url, config.durable.max_payload_bytes)
            .await
            .unwrap();
        backend.init().await.unwrap();
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let step_id = StepId::new(0);
        backend
            .append(JournalEntry {
                seq: None,
                execution_id: exec,
                kind: ExecutionKind::AgentTurn,
                step_id,
                entry: EntryKind::StepResult {
                    idempotency_key: IdempotencyKey::derive(exec, step_id, b"tool:test"),
                    payload: bytes::Bytes::copy_from_slice(&[0u8, 1, 2, 3]),
                    effect: EffectClass::Idempotent,
                    payload_version: 1,
                },
                created_at_ms: 0,
            })
            .await
            .unwrap();

        let refused = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: true,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(
            refused.is_err(),
            "drop-previous must refuse while a matching sealed payload remains"
        );
        assert_eq!(
            load_config_or_default(&config_path).durable.previous_key_id,
            Some(0),
            "the refused drop must not clear previous_key_id"
        );

        let forced = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: true,
                force: true,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(forced.is_ok(), "--force must skip the scan: {forced:?}");
        assert_eq!(
            load_config_or_default(&config_path).durable.previous_key_id,
            None,
            "--force must still clear previous_key_id once it proceeds"
        );
    }

    /// R1: a config that declares `previous_key_id` while the vault has no
    /// `ZEPH_DURABLE_KEY_PREVIOUS` (the config-first crash window) must refuse rather than derive
    /// the old key from whatever `ZEPH_DURABLE_KEY` currently holds.
    #[tokio::test]
    #[serial]
    async fn rotate_key_refuses_on_partial_state() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());

        // Simulate a crash between the config write and the vault write: bump key_id/
        // previous_key_id directly without touching the vault.
        write_durable_config_fields(&config_path, 1, Some(0)).unwrap();

        let result = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(
            result.is_err(),
            "an inconsistent (config declares window, vault has no previous secret) state must \
             refuse rather than guess"
        );
    }

    /// R3: rotating a declared shared database is refused without `--ack-shared-db-drain`, and
    /// proceeds once it is passed.
    #[tokio::test]
    #[serial]
    async fn rotate_key_refuses_on_shared_db_without_ack() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.shared_db = true;
        let toml = toml::to_string_pretty(&config).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml).unwrap();
        std::fs::write(resolve_durable_db_url(&config), []).unwrap();

        let refused = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(
            refused.is_err(),
            "a shared database must refuse rotation without --ack-shared-db-drain"
        );
        assert_eq!(load_config_or_default(&config_path).durable.key_id, 0);

        let acked = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: true,
            },
            Some(&config_path),
        )
        .await;
        assert!(
            acked.is_ok(),
            "--ack-shared-db-drain must allow rotation to proceed: {acked:?}"
        );
        assert_eq!(load_config_or_default(&config_path).durable.key_id, 1);
    }

    // ── Gap-closing tests (team-lead review round, #6447) ──────────────────────────────────

    /// Mandatory gap (impl-critic + tester, independently confirmed): `load_durable_cipher`'s
    /// fail-closed branch is reachable on every ordinary `zeph run`/`resume` via `load_write_cipher`
    /// (called from `runner.rs`/`scheduler_daemon.rs` on every turn), completely independent of
    /// whether `rotate-key`'s own CLI-level partial-state guard ever ran — e.g. a hand-edited config,
    /// or the exact post-config-first-crash state reached outside the CLI. Must hard-error, never
    /// silently skip the previous-key slot.
    #[tokio::test]
    #[serial]
    async fn load_write_cipher_fails_closed_when_previous_key_id_set_but_vault_secret_missing() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();

        let mut config = Config::default();
        config.durable.previous_key_id = Some(0);
        // key_id stays default 0; ZEPH_DURABLE_KEY_PREVIOUS was never written to the vault.

        let result = load_write_cipher(&config);
        assert!(
            result.is_err(),
            "previous_key_id set but ZEPH_DURABLE_KEY_PREVIOUS missing must hard-error, not \
             silently build a cipher with no previous slot"
        );
    }

    /// Companion to the above: a genuinely consistent (config ∧ vault) previous-key pair must
    /// build successfully, so the fail-closed branch above is proven to trigger on the
    /// inconsistency specifically, not on `previous_key_id` being set at all.
    #[tokio::test]
    #[serial]
    async fn load_write_cipher_succeeds_when_previous_key_id_and_vault_secret_are_consistent() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();

        let vault_root = zeph_core::vault::default_vault_dir();
        let mut provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        provider
            .set_secret_mut(
                PREVIOUS_KEY_VAULT_NAME.to_owned(),
                zeph_core::durable::generate_durable_key_b64(),
                false,
            )
            .unwrap();
        provider.save().unwrap();

        let mut config = Config::default();
        config.durable.key_id = 1;
        config.durable.previous_key_id = Some(0);

        let result = load_write_cipher(&config);
        assert!(
            result.is_ok(),
            "a consistent (config ∧ vault) previous-key pair must build a cipher: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    /// Companion: `previous_key_id = None` (the default, no rotation window declared) must skip
    /// the previous-slot lookup cleanly and never touch `ZEPH_DURABLE_KEY_PREVIOUS`.
    #[tokio::test]
    #[serial]
    async fn load_write_cipher_skips_previous_slot_cleanly_when_previous_key_id_is_none() {
        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();

        let config = Config::default();
        assert_eq!(config.durable.previous_key_id, None);

        let result = load_write_cipher(&config);
        assert!(
            result.is_ok(),
            "no declared rotation window must build a cipher with no previous slot: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    /// Mandatory gap: end-to-end path for R5's `describe_reveal_error` mapping — seal a payload
    /// under key-id 0 with the real write-path cipher, open then force-drop a rotation window (so
    /// the previous slot is genuinely gone, mirroring an operator who force-dropped after
    /// confirming pruning by other means), and confirm the `--reveal` read path surfaces the
    /// actionable "predates a key rotation" message instead of the raw `UnknownKeyId`/decode error.
    #[tokio::test]
    #[serial]
    async fn reveal_after_drop_previous_surfaces_actionable_rotation_message() {
        use zeph_durable::{
            EffectClass, EntryKind, ExecutionKind, IdempotencyKey, JournalEntry, StepId,
        };

        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        let toml = toml::to_string_pretty(&config).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml).unwrap();

        // Seal a real payload under key-id 0 with the actual write-path cipher.
        let cipher = load_write_cipher(&config).unwrap().unwrap();
        let url = resolve_durable_db_url(&config);
        let exec = ExecutionId::new();
        {
            let backend = LocalBackend::open(&url, config.durable.max_payload_bytes)
                .await
                .unwrap()
                .with_cipher(cipher);
            backend.init().await.unwrap();
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            let step_id = StepId::new(0);
            backend
                .append(JournalEntry {
                    seq: None,
                    execution_id: exec,
                    kind: ExecutionKind::AgentTurn,
                    step_id,
                    entry: EntryKind::StepResult {
                        idempotency_key: IdempotencyKey::derive(exec, step_id, b"tool:test"),
                        payload: bytes::Bytes::copy_from_slice(b"secret result"),
                        effect: EffectClass::Idempotent,
                        payload_version: 1,
                    },
                    created_at_ms: 0,
                })
                .await
                .unwrap();
        }

        // Open, then force-drop the rotation window; the sealed blob is never pruned in this test.
        handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await
        .unwrap();
        handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: true,
                force: true,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        let final_config = load_config_or_default(&config_path);
        assert_eq!(final_config.durable.previous_key_id, None);
        let backend = open_backend(&final_config, true).await.unwrap().unwrap();
        let result = show_entries(&backend, exec, true, None, false).await;

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("predates a key rotation"),
            "expected the actionable rotation-restart message, got: {err}"
        );
    }

    /// Nice-to-have gap: `--drop-previous --dry-run` with a window open and a surviving sealed
    /// payload must still refuse (the blob-scan runs even under `--dry-run`, per the documented
    /// design decision that a dry-run should accurately report whether the drop would be
    /// refused) and must not mutate config or vault.
    #[tokio::test]
    #[serial]
    async fn rotate_key_drop_previous_dry_run_still_refuses_with_surviving_blob() {
        use zeph_durable::{
            EffectClass, EntryKind, ExecutionKind, IdempotencyKey, JournalEntry, StepId,
        };

        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_toml(dir.path());

        handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        let config = load_config_or_default(&config_path);
        let url = resolve_durable_db_url(&config);
        let backend = LocalBackend::open(&url, config.durable.max_payload_bytes)
            .await
            .unwrap();
        backend.init().await.unwrap();
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let step_id = StepId::new(0);
        backend
            .append(JournalEntry {
                seq: None,
                execution_id: exec,
                kind: ExecutionKind::AgentTurn,
                step_id,
                entry: EntryKind::StepResult {
                    idempotency_key: IdempotencyKey::derive(exec, step_id, b"tool:test"),
                    payload: bytes::Bytes::copy_from_slice(&[0u8, 9, 9, 9]),
                    effect: EffectClass::Idempotent,
                    payload_version: 1,
                },
                created_at_ms: 0,
            })
            .await
            .unwrap();

        let before_vault = {
            let vault_root = zeph_core::vault::default_vault_dir();
            let provider = zeph_core::vault::AgeVaultProvider::load(
                &vault_root.join("vault-key.txt"),
                &vault_root.join("secrets.age"),
            )
            .unwrap();
            provider.get(PREVIOUS_KEY_VAULT_NAME).map(str::to_owned)
        };

        let result = handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: true,
                drop_previous: true,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await;
        assert!(
            result.is_err(),
            "--drop-previous --dry-run must still refuse when a matching sealed payload remains"
        );
        assert_eq!(
            load_config_or_default(&config_path).durable.previous_key_id,
            Some(0),
            "the refused dry-run drop must not clear previous_key_id"
        );

        let after_vault = {
            let vault_root = zeph_core::vault::default_vault_dir();
            let provider = zeph_core::vault::AgeVaultProvider::load(
                &vault_root.join("vault-key.txt"),
                &vault_root.join("secrets.age"),
            )
            .unwrap();
            provider.get(PREVIOUS_KEY_VAULT_NAME).map(str::to_owned)
        };
        assert_eq!(
            before_vault, after_vault,
            "the refused dry-run drop must not mutate the vault"
        );
    }

    /// Recommended gap: end-to-end round trip through the real CLI + backend — seal a payload
    /// under key-id 0 with the real cipher, rotate via the CLI, confirm the pre-rotation entry
    /// still decrypts through the registered previous slot, and confirm a fresh write after
    /// rotation carries the new key-id on disk. This is the architect's original primary
    /// acceptance scenario for the feature.
    #[tokio::test]
    #[serial]
    async fn rotate_key_round_trip_old_blob_still_decrypts_and_new_writes_use_new_key_id() {
        use zeph_durable::{
            EffectClass, EntryKind, ExecutionKind, IdempotencyKey, JournalEntry, StepId,
        };

        let _guard = VaultDirGuard::new();
        seed_vault_with_durable_key();

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        let toml = toml::to_string_pretty(&config).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml).unwrap();

        let old_cipher = load_write_cipher(&config).unwrap().unwrap();
        let url = resolve_durable_db_url(&config);
        let exec = ExecutionId::new();
        {
            let backend = LocalBackend::open(&url, config.durable.max_payload_bytes)
                .await
                .unwrap()
                .with_cipher(old_cipher);
            backend.init().await.unwrap();
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            let step_id = StepId::new(0);
            backend
                .append(JournalEntry {
                    seq: None,
                    execution_id: exec,
                    kind: ExecutionKind::AgentTurn,
                    step_id,
                    entry: EntryKind::StepResult {
                        idempotency_key: IdempotencyKey::derive(exec, step_id, b"tool:old"),
                        payload: bytes::Bytes::copy_from_slice(b"pre-rotation result"),
                        effect: EffectClass::Idempotent,
                        payload_version: 1,
                    },
                    created_at_ms: 0,
                })
                .await
                .unwrap();
        }

        handle_durable_command(
            DurableCommand::RotateKey {
                dry_run: false,
                drop_previous: false,
                force: false,
                ack_shared_db_drain: false,
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        let rotated_config = load_config_or_default(&config_path);
        assert_eq!(rotated_config.durable.key_id, 1);
        assert_eq!(rotated_config.durable.previous_key_id, Some(0));

        let new_cipher = load_write_cipher(&rotated_config).unwrap().unwrap();
        let backend = LocalBackend::open(&url, rotated_config.durable.max_payload_bytes)
            .await
            .unwrap()
            .with_cipher(new_cipher);

        // A fresh write after rotation must carry the new key-id on disk.
        let new_step_id = StepId::new(1);
        backend
            .append(JournalEntry {
                seq: None,
                execution_id: exec,
                kind: ExecutionKind::AgentTurn,
                step_id: new_step_id,
                entry: EntryKind::StepResult {
                    idempotency_key: IdempotencyKey::derive(exec, new_step_id, b"tool:new"),
                    payload: bytes::Bytes::copy_from_slice(b"post-rotation result"),
                    effect: EffectClass::Idempotent,
                    payload_version: 1,
                },
                created_at_ms: 1,
            })
            .await
            .unwrap();
        let (new_payload,): (Option<Vec<u8>>,) = zeph_db::query_as(zeph_db::sql!(
            "SELECT payload FROM durable_journal WHERE execution_id = ? AND step_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .bind(1i64)
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(
            new_payload.unwrap()[0],
            1,
            "seals written after rotation must carry the rotated key-id"
        );

        // The pre-rotation entry must still decrypt via the registered previous slot.
        let entries = backend.read_execution(exec).await.unwrap();
        let old_entry = entries
            .iter()
            .find(|e| e.step_id.value() == 0)
            .expect("pre-rotation entry must still be present");
        match &old_entry.entry {
            EntryKind::StepResult { payload, .. } => {
                assert_eq!(payload.as_ref(), b"pre-rotation result");
            }
            other => panic!("unexpected entry kind: {other:?}"),
        }
    }
}
