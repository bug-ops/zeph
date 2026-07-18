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
use zeph_core::config::Config;
use zeph_core::durable::XChaCha20Poly1305Cipher;
use zeph_core::vault::AgeVaultProvider;
use zeph_durable::{CancelOutcome, ExecutionId, Journal, LocalBackend};

use crate::cli::DurableCommand;

/// Printed before any decrypted payload is shown, so the operator knows plaintext is on screen.
const REVEAL_WARNING: &str =
    "WARNING: --reveal decrypts and prints payload bytes in cleartext. Do not share this output.";

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

/// Load the AEAD cipher from the vault-stored `ZEPH_DURABLE_KEY` for `--reveal`.
fn load_durable_cipher() -> anyhow::Result<XChaCha20Poly1305Cipher> {
    let dir = zeph_core::vault::default_vault_dir();
    let provider = AgeVaultProvider::load(&dir.join("vault-key.txt"), &dir.join("secrets.age"))
        .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
    let key = provider.get("ZEPH_DURABLE_KEY").ok_or_else(|| {
        anyhow::anyhow!("ZEPH_DURABLE_KEY not found in vault; cannot --reveal payloads")
    })?;
    XChaCha20Poly1305Cipher::from_vault_b64(key)
        .map_err(|e| anyhow::anyhow!("invalid ZEPH_DURABLE_KEY: {e}"))
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
    let cipher = load_durable_cipher()?;
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
        let cipher = load_durable_cipher()?;
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
    }

    Ok(())
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
        let mut entries = backend
            .read_execution(exec)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read execution: {e}"))?;
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
}
