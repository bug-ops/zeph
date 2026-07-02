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
use zeph_durable::{ExecutionId, Journal, LocalBackend};

use crate::cli::DurableCommand;

/// Printed before any decrypted payload is shown, so the operator knows plaintext is on screen.
const REVEAL_WARNING: &str =
    "WARNING: --reveal decrypts and prints payload bytes in cleartext. Do not share this output.";

/// Resolve the dedicated `durable.db` path: a sibling of the main database file.
///
/// The durable journal lives in its own file on its own pool (INV-14), next to `memory.sqlite_path`.
/// Shared with the TUI durable poll task so both target the same file.
pub(crate) fn resolve_durable_db_url(config: &Config) -> String {
    let main = config.memory.sqlite_path.as_str();
    match Path::new(main).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            dir.join("durable.db").to_string_lossy().into_owned()
        }
        _ => "durable.db".to_owned(),
    }
}

/// Format a Unix-epoch-millisecond timestamp as a readable UTC string, falling back to the raw value.
fn fmt_ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
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

/// Resolve the AEAD payload cipher to attach on a durable *write* path when
/// `config.durable.encrypt_payload` is enabled (INV-5).
///
/// Returns `Ok(None)` when encryption is disabled — the documented dev-only override where
/// payloads are stored as plaintext, so no cipher is attached and writes stay unchanged.
/// Returns `Ok(Some(cipher))` when encryption is enabled and `ZEPH_DURABLE_KEY` resolves from
/// the vault. Fails closed (`Err`) when encryption is enabled but the key is missing: a write
/// path must never silently persist plaintext while the config declares payloads encrypted.
pub(crate) fn load_write_cipher(
    config: &Config,
) -> anyhow::Result<Option<Arc<dyn zeph_durable::PayloadCipher>>> {
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
/// Returns `Ok(None)` when no journal file exists yet (a friendly signal that durable execution has
/// not run on this deployment), so the caller can print guidance instead of creating an empty file.
async fn open_backend(config: &Config, reveal: bool) -> anyhow::Result<Option<LocalBackend>> {
    let url = resolve_durable_db_url(config);
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
                let n = backend
                    .count_prunable(policy)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to count prunable executions: {e}"))?;
                println!("Dry run: {n} terminal execution(s) past TTL would be pruned.");
            } else {
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
    fn durable_db_is_sibling_of_sqlite_path() {
        let mut config = Config::default();
        config.memory.sqlite_path = "/data/zeph/zeph.db".to_owned();
        assert_eq!(resolve_durable_db_url(&config), "/data/zeph/durable.db");
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
        provider.set_secret_mut(
            "ZEPH_DURABLE_KEY".to_owned(),
            zeph_core::durable::generate_durable_key_b64(),
        );
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
}
